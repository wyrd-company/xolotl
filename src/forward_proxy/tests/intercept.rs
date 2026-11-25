//! Interception against a local TLS origin, with certificates from the test PKI.

use std::convert::Infallible;
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{Request as HttpRequest, Response as HttpResponse};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::rustls::pki_types::ServerName;

use super::*;
use crate::context::SessionId;
use crate::forward_proxy::capture::{CaptureHold, Forwarded};
use crate::forward_proxy::testpki::TestPki;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestBody = UnsyncBoxBody<Bytes, BoxError>;
type Handler = Arc<dyn Fn(HttpRequest<Incoming>) -> BoxFuture<'static, HttpResponse<TestBody>> + Send + Sync>;

const MODEL_PATH: &str = "/v1internal:streamGenerateContent";
const UPSTREAM_TOKEN: &str = "Bearer ya29.upstream-fixture";
const REQUEST_BODY: &str = r#"{"model":"gemini-3-pro","project":"p","request":{"session_id":"sess-1","contents":[{"role":"user","parts":[{"text":"PING!"}]}],"generationConfig":{"model":"not-this"}}}"#;
const SSE_BODY: &str = concat!(
    "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"PONG\"}]}}]},\"traceId\":\"t\"}\r\n\r\n",
    "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{}}}]},\"finishReason\":\"STOP\"}],",
    "\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":2,\"cachedContentTokenCount\":4}},\"traceId\":\"t\"}\r\n\r\n",
);

fn full(body: impl Into<Bytes>) -> TestBody {
    Full::new(body.into()).map_err(|never| match never {}).boxed_unsync()
}

/// A CA of the test's own.
fn ca(name: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

/// What the origin received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    version: http::Version,
    headers: http::HeaderMap,
    body: Bytes,
}

/// A local TLS origin for the capture host, its certificate from a CA of its own.
struct Origin {
    address: SocketAddr,
    ca: CertificateDer<'static>,
    handshakes: Arc<AtomicUsize>,
}

impl Origin {
    async fn start(alpn: &[&[u8]], handler: Handler) -> Self {
        let ca = ca("origin test CA");
        let leaf = CertificateParams::new(vec![CAPTURE.to_owned()]).unwrap();
        let key = KeyPair::generate().unwrap();
        let leaf = leaf.signed_by(&key, &ca).unwrap();
        let mut tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone()], PrivateKeyDer::try_from(key.serialize_der()).unwrap())
            .unwrap();
        tls.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handshakes = Arc::new(AtomicUsize::new(0));
        let count = handshakes.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (acceptor, handler, count) = (acceptor.clone(), handler.clone(), count.clone());
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else { return };
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let service = hyper::service::service_fn(move |request| {
                        let response = handler(request);
                        async move { Ok::<_, Infallible>(response.await) }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { address, ca: ca.der().clone(), handshakes }
    }
}

/// Records each request, and answers the model path with [`SSE_BODY`].
fn recording(seen: Arc<Mutex<Vec<Seen>>>) -> Handler {
    Arc::new(move |request: HttpRequest<Incoming>| {
        let seen = seen.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            let target = parts.uri.path_and_query().unwrap().to_string();
            seen.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                target: target.clone(),
                version: parts.version,
                headers: parts.headers,
                body,
            });
            let builder = HttpResponse::builder()
                .header("x-goog-safety", "checked")
                .header("set-cookie", "upstream=1")
                .header("alt-svc", "h3=\":443\"; ma=2592000");
            if target.starts_with(MODEL_PATH) {
                builder.header("content-type", "text/event-stream").body(full(SSE_BODY)).unwrap()
            } else if target.starts_with("/gzip") {
                // An opaque coded body: forwarded as sent.
                builder
                    .header("content-type", "application/json")
                    .header("content-encoding", "gzip")
                    .body(full(&b"\x1f\x8b\x08\x00opaque"[..]))
                    .unwrap()
            } else {
                builder.header("content-type", "application/json").body(full("{}")).unwrap()
            }
        })
    })
}

/// A capture sink that sends each [`Forwarded`] to the test, or panics.
struct TestSink {
    limit: Option<usize>,
    tx: mpsc::UnboundedSender<Forwarded>,
    holds: Arc<AtomicUsize>,
    panic: bool,
}

impl TestSink {
    fn new(limit: Option<usize>, panic: bool) -> (Arc<Self>, mpsc::UnboundedReceiver<Forwarded>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(Self { limit, tx, holds: Default::default(), panic }), rx)
    }
}

impl CaptureSink for TestSink {
    fn body_limit(&self) -> Option<usize> {
        self.limit
    }

    fn hold(&self) -> CaptureHold {
        self.holds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, panic) = (self.tx.clone(), self.panic);
        CaptureHold::new(move |forwarded| {
            assert!(!panic, "the capture consumer failed");
            tx.send(forwarded).unwrap();
        })
    }
}

struct Setup {
    proxy: Proxy,
    pki: TestPki,
    key: String,
    h: crate::server::tests::Harness,
}

async fn setup(origin: &Origin, capture: Option<Arc<dyn CaptureSink>>, idle: Duration) -> Setup {
    setup_with(origin, capture, idle, origin.ca.clone()).await
}

/// A proxy whose capture host is `origin`, verified against `upstream_ca`.
async fn setup_with(origin: &Origin, capture: Option<Arc<dyn CaptureSink>>, idle: Duration, upstream_ca: CertificateDer<'static>) -> Setup {
    let h = harness().await;
    let key = h.key(egress_policy()).await;
    let pki = TestPki::start(&[CAPTURE], &[CAPTURE]).await;
    let dialer = Arc::new(TestDialer { upstream: Some(origin.address), ..Default::default() });
    let interception =
        Interception { capture_hosts: vec![CAPTURE.into()], vault: Some(pki.vault()), capture, upstream_ca: Some(upstream_ca) };
    let proxy = Proxy::start_with(h.state.keys.clone(), dialer, idle, interception).await;
    Setup { proxy, pki, key, h }
}

/// A client whose proxy is the test proxy, trusting only the listener's
/// certificate and `roots`.
fn client(proxy: &Proxy, key: &str, roots: &[&CertificateDer<'static>], http1: bool) -> reqwest::Client {
    let mut certs = vec![reqwest::Certificate::from_der(&proxy.tls.cert).unwrap()];
    certs.extend(roots.iter().map(|root| reqwest::Certificate::from_der(root).unwrap()));
    let builder = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("https://client:{key}@{}", proxy.address)).unwrap())
        .tls_certs_only(certs)
        .timeout(TEST_TIMEOUT);
    if http1 { builder.http1_only() } else { builder }.build().unwrap()
}

fn model_request(client: &reqwest::Client) -> reqwest::RequestBuilder {
    client
        .post(format!("https://{CAPTURE}{MODEL_PATH}?alt=sse"))
        .header("authorization", UPSTREAM_TOKEN)
        .header("cookie", "session=cookie-fixture")
        .header("x-goog-api-client", "gl-node/22.0.0")
        .header("user-agent", "antigravity-test/1.0")
        .header("accept-encoding", "gzip, deflate, br, zstd")
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
}

async fn next_capture(rx: &mut mpsc::UnboundedReceiver<Forwarded>) -> Forwarded {
    timeout(TEST_TIMEOUT, rx.recv()).await.expect("a captured request").unwrap()
}

#[tokio::test]
async fn intercepts_a_capture_host_and_forwards_unchanged() {
    for http1 in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let alpn: &[&[u8]] = if http1 { &[b"http/1.1"] } else { &[b"h2", b"http/1.1"] };
        let origin = Origin::start(alpn, recording(seen.clone())).await;
        let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
        let s = setup(&origin, Some(sink.clone()), TEST_TIMEOUT).await;
        // The client trusts the test PKI, not the origin's CA: the certificate it sees is xolotl's.
        let client = client(&s.proxy, &s.key, &[&s.pki.root], http1);

        let mut request = model_request(&client);
        if http1 {
            request = request.header("connection", "x-hop").header("x-hop", "dropped");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), if http1 { http::Version::HTTP_11 } else { http::Version::HTTP_2 });
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["x-goog-safety"], "checked");
        assert_eq!(response.headers()["set-cookie"], "upstream=1");
        assert!(response.headers().get("alt-svc").is_none());
        assert_eq!(response.text().await.unwrap(), SSE_BODY);

        let coded = client.get(format!("https://{CAPTURE}/gzip")).header("accept-encoding", "gzip").send().await.unwrap();
        assert_eq!(coded.headers()["content-encoding"], "gzip");
        assert_eq!(coded.bytes().await.unwrap(), &b"\x1f\x8b\x08\x00opaque"[..]);
        let other = client.get(format!("https://{CAPTURE}/v1internal:loadCodeAssist")).send().await.unwrap();
        assert_eq!(other.status(), 200);
        let options = client.request(reqwest::Method::OPTIONS, format!("https://{CAPTURE}{MODEL_PATH}")).send().await.unwrap();
        assert_eq!(options.status(), 200);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        let model = &seen[0];
        assert_eq!((model.method.as_str(), model.target.as_str()), ("POST", "/v1internal:streamGenerateContent?alt=sse"));
        assert_eq!(model.version, if http1 { http::Version::HTTP_11 } else { http::Version::HTTP_2 });
        assert_eq!(model.body, REQUEST_BODY.as_bytes());
        for (name, value) in [
            ("authorization", UPSTREAM_TOKEN),
            ("cookie", "session=cookie-fixture"),
            ("x-goog-api-client", "gl-node/22.0.0"),
            ("user-agent", "antigravity-test/1.0"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("content-type", "application/json"),
        ] {
            assert_eq!(model.headers.get(name).map(|v| v.to_str().unwrap()), Some(value), "{name}");
        }
        for name in ["proxy-authorization", "x-hop", "connection"] {
            assert!(model.headers.get(name).is_none(), "{name} reached the origin");
        }
        assert_eq!(seen[3].method, "OPTIONS");

        // One model request, one capture: in its own task, with no header and no query string.
        let row = next_capture(&mut rows).await;
        assert_eq!((row.client_id.as_str(), row.host.as_str(), row.path.as_str()), ("egress/client", CAPTURE, MODEL_PATH));
        assert_eq!((row.status, row.error.as_deref()), (200, None));
        assert!(row.started_at <= chrono::Utc::now() && row.latency_ms < TEST_TIMEOUT.as_millis() as u64);
        assert!(row.trace_id.is_empty() || row.trace_id.len() == 32, "{}", row.trace_id);
        assert_eq!(row.request.bytes, REQUEST_BODY.as_bytes());
        assert_eq!(row.response.bytes, SSE_BODY.as_bytes());
        assert!(row.response.sse && !row.request.truncated && !row.response.truncated);
        assert_eq!(row.observed.model.as_deref(), Some("gemini-3-pro"));
        assert_eq!(row.observed.session_key, Some(SessionId::sent("cloud-code", "sess-1").key));
        assert_eq!(row.observed.usage, crate::proxy::TokenUsage { input: Some(10), output: Some(5), cached: Some(4), cache_write: None });
        assert_eq!(row.observed.tool_calls, ["read_file"]);
        assert!(row.observed.complete);
        let text = format!("{row:?}");
        for secret in ["ya29", "cookie-fixture", "alt=sse", "gl-node", "sess-1"] {
            assert!(!text.contains(secret), "{secret} is in the capture");
        }
        assert!(rows.try_recv().is_err(), "only the model request is captured");
        assert_eq!(sink.holds.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(*s.pki.requests.lock().unwrap(), [CAPTURE]);
        s.proxy.stop().await;
        drop(s.h);
    }
}

/// An SSE response of two chunks; the second waits for `release`.
fn two_chunks(release: oneshot::Receiver<()>) -> Handler {
    let release = Arc::new(Mutex::new(Some(release)));
    Arc::new(move |_request| {
        let release = release.clone();
        Box::pin(async move {
            let release = release.lock().unwrap().take().unwrap();
            let chunks = futures::stream::unfold((0, Some(release)), |(step, release)| async move {
                match step {
                    0 => Some((Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"data: {\"n\":1}\n\n"))), (1, release))),
                    1 => {
                        release.unwrap().await.unwrap();
                        Some((Ok(Frame::data(Bytes::from_static(b"data: {\"n\":2}\n\n"))), (2, None)))
                    }
                    _ => None,
                }
            });
            HttpResponse::builder().header("content-type", "text/event-stream").body(StreamBody::new(chunks).boxed_unsync()).unwrap()
        })
    })
}

#[tokio::test]
async fn streamed_chunks_reach_the_client_as_they_arrive() {
    // The second chunk waits until the client has read the first.
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let handler = two_chunks(release_rx);
    let origin = Origin::start(&[b"h2", b"http/1.1"], handler).await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    let s = setup(&origin, Some(sink), TEST_TIMEOUT).await;
    let client = client(&s.proxy, &s.key, &[&s.pki.root], false);
    let mut response = model_request(&client).send().await.unwrap();
    let first = timeout(TEST_TIMEOUT, response.chunk()).await.expect("the first chunk before the second is sent").unwrap();
    assert_eq!(first.as_deref(), Some(&b"data: {\"n\":1}\n\n"[..]));
    release_tx.send(()).unwrap();
    let second = timeout(TEST_TIMEOUT, response.chunk()).await.unwrap().unwrap();
    assert_eq!(second.as_deref(), Some(&b"data: {\"n\":2}\n\n"[..]));
    while let Some(rest) = response.chunk().await.unwrap() {
        assert!(rest.is_empty(), "{rest:?}");
    }
    assert_eq!(next_capture(&mut rows).await.response.bytes, b"data: {\"n\":1}\n\ndata: {\"n\":2}\n\n");
    s.proxy.stop().await;
}

#[tokio::test]
async fn an_upstream_that_fails_validation_fails_the_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = Origin::start(&[b"h2", b"http/1.1"], recording(seen.clone())).await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    // The upstream verifier trusts a third CA, not the origin's.
    let s = setup_with(&origin, Some(sink), TEST_TIMEOUT, ca("untrusted test CA").der().clone()).await;
    let client = client(&s.proxy, &s.key, &[&s.pki.root], false);

    let response = model_request(&client).send().await.unwrap();
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], 502);
    assert_eq!(body["error"]["status"], "UNAVAILABLE");
    assert!(body["error"]["message"].as_str().unwrap().contains("UnknownIssuer"), "{body}");
    assert!(seen.lock().unwrap().is_empty(), "nothing reached the origin");
    assert_eq!(origin.handshakes.load(std::sync::atomic::Ordering::SeqCst), 0);
    let row = next_capture(&mut rows).await;
    assert_eq!(row.status, 502);
    assert!(row.error.unwrap().starts_with("proxy_error: can't reach the upstream"));
    s.proxy.stop().await;
}

#[tokio::test]
async fn a_failing_capture_consumer_leaves_the_request_working() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = Origin::start(&[b"h2", b"http/1.1"], recording(seen.clone())).await;
    let (sink, _rows) = TestSink::new(Some(1 << 20), true);
    let s = setup(&origin, Some(sink.clone()), TEST_TIMEOUT).await;
    let client = client(&s.proxy, &s.key, &[&s.pki.root], false);
    for _ in 0..3 {
        let response = model_request(&client).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), SSE_BODY);
    }
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_eq!(sink.holds.load(std::sync::atomic::Ordering::SeqCst), 3);
    s.proxy.stop().await;
}

#[tokio::test]
async fn a_name_outside_the_pki_role_is_tunneled_not_intercepted() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = Origin::start(&[b"h2", b"http/1.1"], recording(seen.clone())).await;
    let h = harness().await;
    let key = h.key(egress_policy()).await;
    // The role signs only another name, so the capture host gets no certificate.
    let pki = TestPki::start(&["other.example"], &["other.example"]).await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    let dialer = Arc::new(TestDialer { upstream: Some(origin.address), ..Default::default() });
    let interception = Interception {
        capture_hosts: vec![CAPTURE.into()],
        vault: Some(pki.vault()),
        capture: Some(sink),
        upstream_ca: Some(origin.ca.clone()),
    };
    let proxy = Proxy::start_with(h.state.keys.clone(), dialer, TEST_TIMEOUT, interception).await;

    // A client that trusts only the test PKI can't complete TLS: the connection is a tunnel to the origin.
    let pki_only = client(&proxy, &key, &[&pki.root], false);
    assert!(model_request(&pki_only).send().await.is_err());
    let direct = client(&proxy, &key, &[&origin.ca], false);
    assert_eq!(model_request(&direct).send().await.unwrap().text().await.unwrap(), SSE_BODY);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(pki.requests.lock().unwrap().iter().all(|name| name == CAPTURE));
    assert!(timeout(Duration::from_millis(200), rows.recv()).await.is_err(), "a tunnel is not captured");
    proxy.stop().await;
}

#[tokio::test]
async fn only_the_connect_host_is_served() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin = Origin::start(&[b"http/1.1"], recording(seen.clone())).await;
    let s = setup(&origin, None, TEST_TIMEOUT).await;

    // A non-capture host causes no certificate request.
    let mut stream = s.proxy.stream().await;
    stream.write_all(connect_request(&format!("{ORIGIN}:443"), &s.key).as_bytes()).await.unwrap();
    let _ = timeout(TEST_TIMEOUT, read_response_head(&mut stream)).await.unwrap();
    assert!(!s.pki.requests.lock().unwrap().iter().any(|name| name == ORIGIN));

    // A client's SNI must be the CONNECT host.
    let mut stream = s.proxy.stream().await;
    stream.write_all(connect_request(&format!("{CAPTURE}:443"), &s.key).as_bytes()).await.unwrap();
    let stream = timeout(TEST_TIMEOUT, read_response_head(stream)).await.unwrap().into_inner();
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(s.pki.root.clone()).unwrap();
    let tls = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let name = ServerName::try_from("other.example").unwrap();
    assert!(timeout(TEST_TIMEOUT, TlsConnector::from(Arc::new(tls)).connect(name, stream)).await.unwrap().is_err());

    // A request whose Host names another host is misdirected.
    let client = client(&s.proxy, &s.key, &[&s.pki.root], true);
    let response = client.get(format!("https://{CAPTURE}/v1internal:loadCodeAssist")).header("host", "other.example").send().await.unwrap();
    assert_eq!(response.status(), 421);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], 421);
    assert!(seen.lock().unwrap().is_empty());

    // A key that stops passing gets 403 on its next request, and the connection closes.
    assert_eq!(client.get(format!("https://{CAPTURE}/v1internal:loadCodeAssist")).send().await.unwrap().status(), 200);
    s.h.state.keys.revoke("egress/client").await.unwrap();
    let refused = client.get(format!("https://{CAPTURE}/v1internal:loadCodeAssist")).send().await.unwrap();
    assert_eq!(refused.status(), 403);
    assert_eq!(refused.headers()["connection"], "close");
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["status"], "PERMISSION_DENIED");
    // A new connection now gets 407 at its CONNECT.
    assert!(client.get(format!("https://{CAPTURE}/v1internal:loadCodeAssist")).send().await.is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);
    s.proxy.stop().await;
}

#[tokio::test]
async fn an_upgrade_is_joined_as_a_tunnel() {
    let handler: Handler = Arc::new(|mut request: HttpRequest<Incoming>| {
        Box::pin(async move {
            assert_eq!(request.headers()["upgrade"], "websocket");
            assert_eq!(request.headers()["connection"], "upgrade");
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut io = TokioIo::new(upgrade.await.unwrap());
                let mut ping = [0; 4];
                io.read_exact(&mut ping).await.unwrap();
                io.write_all(b"pong").await.unwrap();
            });
            HttpResponse::builder()
                .status(101)
                .header("upgrade", "websocket")
                .header("connection", "upgrade")
                .header("x-origin", "kept")
                .body(full(""))
                .unwrap()
        })
    });
    let origin = Origin::start(&[b"h2", b"http/1.1"], handler).await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    let s = setup(&origin, Some(sink), TEST_TIMEOUT).await;
    let mut stream = s.proxy.stream().await;
    stream.write_all(connect_request(&format!("{CAPTURE}:443"), &s.key).as_bytes()).await.unwrap();
    let stream = timeout(TEST_TIMEOUT, read_response_head(stream)).await.unwrap().into_inner();
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(s.pki.root.clone()).unwrap();
    let mut tls = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let stream = TlsConnector::from(Arc::new(tls)).connect(ServerName::try_from(CAPTURE).unwrap(), stream).await.unwrap();
    let mut stream = BufReader::new(stream);
    let request =
        format!("POST {MODEL_PATH} HTTP/1.1\r\nHost: {CAPTURE}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nContent-Length: 0\r\n\r\n");
    stream.get_mut().write_all(request.as_bytes()).await.unwrap();
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        assert_ne!(timeout(TEST_TIMEOUT, stream.read_line(&mut head)).await.unwrap().unwrap(), 0);
    }
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("\r\nupgrade: websocket\r\n") && lower.contains("\r\nconnection: upgrade\r\n"), "{head}");
    stream.get_mut().write_all(b"ping").await.unwrap();
    let mut pong = [0; 4];
    timeout(TEST_TIMEOUT, stream.read_exact(&mut pong)).await.unwrap().unwrap();
    assert_eq!(&pong, b"pong");
    assert!(timeout(Duration::from_millis(200), rows.recv()).await.is_err(), "an upgrade is not captured");
    s.proxy.stop().await;
}

/// An HTTP/1.1 origin that reads a request, then does what its `case` query
/// says: `close` before any headers, `silent` with no answer, `cut` the body
/// short, or `stall` in the body.
async fn misbehaving_origin() -> Origin {
    let ca = ca("origin test CA");
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![CAPTURE.to_owned()]).unwrap().signed_by(&key, &ca).unwrap();
    let mut tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], PrivateKeyDer::try_from(key.serialize_der()).unwrap())
        .unwrap();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else { return };
                let mut stream = BufReader::new(stream);
                let mut head = String::new();
                while !head.ends_with("\r\n\r\n") {
                    if stream.read_line(&mut head).await.unwrap_or(0) == 0 {
                        return;
                    }
                }
                let length = head
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let case = head.split_once("case=").map(|(_, rest)| rest.split([' ', '&']).next().unwrap().to_owned()).unwrap_or_default();
                let stream = stream.get_mut();
                let partial = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{\"a\":";
                match case.as_str() {
                    "close" => {}
                    "silent" => std::future::pending::<()>().await,
                    "cut" => stream.write_all(partial.as_bytes()).await.unwrap(),
                    "stall" => {
                        stream.write_all(partial.as_bytes()).await.unwrap();
                        stream.flush().await.unwrap();
                        std::future::pending::<()>().await;
                    }
                    other => panic!("unknown case {other}"),
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    Origin { address, ca: ca.der().clone(), handshakes: Default::default() }
}

#[tokio::test]
async fn each_failure_gives_its_answer_and_its_row() {
    let origin = misbehaving_origin().await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    let s = setup(&origin, Some(sink), Duration::from_secs(1)).await;
    for http1 in [false, true] {
        let client = client(&s.proxy, &s.key, &[&s.pki.root], http1);
        let call = |case: &str| {
            client
                .post(format!("https://{CAPTURE}/v1internal:generateContent?case={case}"))
                .header("content-type", "application/json")
                .body(r#"{"model":"m"}"#)
                .send()
        };

        let response = call("close").await.unwrap();
        assert_eq!(response.status(), 502);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            (body["error"]["status"].as_str(), body["error"]["message"].as_str()),
            (Some("UNAVAILABLE"), Some("upstream closed before response headers"))
        );
        let row = next_capture(&mut rows).await;
        assert_eq!((row.status, row.error.as_deref()), (502, Some("proxy_error: upstream closed before response headers")));

        let response = call("silent").await.unwrap();
        assert_eq!(response.status(), 504);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["status"], "DEADLINE_EXCEEDED");
        let row = next_capture(&mut rows).await;
        assert_eq!(row.status, 504);
        assert!(row.error.unwrap().contains("no response headers within 1 s"));

        for (case, error) in [("cut", "proxy_error: upstream read failed"), ("stall", "proxy_error: no data from the upstream for 1 s")] {
            let response = call(case).await.unwrap();
            assert_eq!(response.status(), 200);
            assert!(response.bytes().await.is_err(), "{case}: the client never sees a complete body");
            let row = next_capture(&mut rows).await;
            assert_eq!(row.status, 200, "{case}");
            assert!(row.error.as_deref().unwrap().starts_with(error), "{case}: {:?}", row.error);
            assert_eq!(row.response.bytes, b"{\"a\":", "{case}: the partial copy");
        }

        // The client goes away after the status.
        let response = call("stall").await.unwrap();
        assert_eq!(response.status(), 200);
        drop(response);
        let row = next_capture(&mut rows).await;
        assert_eq!(row.status, 200);
        assert_eq!(row.error.as_deref(), Some("proxy_error: the connection closed before the response ended"));
    }
    s.proxy.stop().await;
}

#[tokio::test]
async fn shutdown_lets_a_request_in_flight_finish() {
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let origin = Origin::start(&[b"h2", b"http/1.1"], two_chunks(release_rx)).await;
    let (sink, mut rows) = TestSink::new(Some(1 << 20), false);
    let s = setup(&origin, Some(sink), TEST_TIMEOUT).await;
    let client = client(&s.proxy, &s.key, &[&s.pki.root], false);
    let mut response = model_request(&client).send().await.unwrap();
    assert!(timeout(TEST_TIMEOUT, response.chunk()).await.unwrap().unwrap().is_some());
    s.proxy.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    release_tx.send(()).unwrap();
    assert_eq!(timeout(TEST_TIMEOUT, response.chunk()).await.unwrap().unwrap().as_deref(), Some(&b"data: {\"n\":2}\n\n"[..]));
    let row = next_capture(&mut rows).await;
    assert_eq!((row.status, row.error), (200, None));
    timeout(TEST_TIMEOUT, s.proxy.task).await.unwrap().unwrap().unwrap();
    // The listener has stopped.
    assert!(model_request(&client).send().await.is_err());
}

#[tokio::test]
async fn a_revoked_key_closes_an_idle_intercepted_connection() {
    let origin = Origin::start(&[b"http/1.1"], recording(Default::default())).await;
    let s = setup(&origin, None, Duration::from_secs(60)).await;
    let mut stream = s.proxy.stream().await;
    stream.write_all(connect_request(&format!("{CAPTURE}:443"), &s.key).as_bytes()).await.unwrap();
    let stream = timeout(TEST_TIMEOUT, read_response_head(stream)).await.unwrap().into_inner();
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(s.pki.root.clone()).unwrap();
    let tls = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let stream = TlsConnector::from(Arc::new(tls)).connect(ServerName::try_from(CAPTURE).unwrap(), stream).await.unwrap();
    // One answered request shows the connection is being served.
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(format!("GET /v1internal:loadCodeAssist HTTP/1.1\r\nHost: {CAPTURE}\r\n\r\n").as_bytes()).await.unwrap();
    let mut head = String::new();
    while !head.ends_with("\r\n\r\n") {
        assert_ne!(timeout(TEST_TIMEOUT, stream.read_line(&mut head)).await.unwrap().unwrap(), 0);
    }
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let mut body = [0; 2];
    stream.read_exact(&mut body).await.unwrap();
    s.h.state.keys.revoke("egress/client").await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(15)).await;
    let mut byte = [0];
    let read = timeout(TEST_TIMEOUT, stream.read(&mut byte)).await.unwrap();
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    tokio::time::resume();
    s.proxy.stop().await;
}
