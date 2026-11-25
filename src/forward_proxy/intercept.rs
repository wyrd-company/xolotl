//! Interception of a capture host: xolotl accepts TLS with the host's
//! certificate and forwards each request, unchanged, to the real host over
//! verified connections that belong to this client connection alone.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Request, Response, StatusCode, Uri, Version};
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use opentelemetry::{KeyValue, metrics::Counter};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, Sleep, timeout};
use tokio_rustls::rustls::client::danger::ServerCertVerifier;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector, client};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info};

use super::capture::{Admission, CaptureSink, Direction, Head, Tap, is_model_request};
use super::destination::{Destination, Dialer};
use super::{Client, PHASE_TIMEOUT, allowed_client, tunnel};
use crate::keys::KeyManager;

/// How long requests in flight get once the connection stops taking new ones.
const DRAIN: Duration = Duration::from_secs(5);

type BoxError = Box<dyn StdError + Send + Sync>;
type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

/// What every intercepted connection of a listener shares.
pub(super) struct Interceptor {
    keys: Arc<KeyManager>,
    dialer: Arc<dyn Dialer>,
    upstream_tls: Arc<ClientConfig>,
    /// For an upgrade, which takes an HTTP/1.1 connection of its own.
    upgrade_tls: Arc<ClientConfig>,
    idle_timeout: Duration,
    capture: Option<Arc<dyn CaptureSink>>,
    max_decoded: usize,
    requests: Counter<u64>,
}

impl Interceptor {
    pub fn new(
        keys: Arc<KeyManager>,
        dialer: Arc<dyn Dialer>,
        verifier: Arc<dyn ServerCertVerifier>,
        idle_timeout: Duration,
        capture: Option<Arc<dyn CaptureSink>>,
        max_decoded: usize,
    ) -> Self {
        let tls = |alpn: &[&[u8]]| {
            let mut tls = ClientConfig::builder().dangerous().with_custom_certificate_verifier(verifier.clone()).with_no_client_auth();
            tls.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
            Arc::new(tls)
        };
        Self {
            keys,
            dialer,
            upstream_tls: tls(&[b"h2", b"http/1.1"]),
            upgrade_tls: tls(&[b"http/1.1"]),
            idle_timeout,
            capture,
            max_decoded,
            requests: opentelemetry::global::meter("xolotl").u64_counter("xolotl.forward_proxy.requests").build(),
        }
    }

    /// Serve one intercepted connection: `stream` has had its `200`, and
    /// `buffered` holds what the client sent after its CONNECT head.
    pub async fn serve<S>(
        self: &Arc<Self>,
        stream: S,
        buffered: Vec<u8>,
        tls: Arc<ServerConfig>,
        destination: &Destination,
        client: &Client,
        shutdown: &CancellationToken,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let started = Instant::now();
        let counts = Arc::new(Counts::default());
        let io = Counted { prefix: buffered, offset: 0, inner: stream, counts: counts.clone() };
        let accept = timeout(PHASE_TIMEOUT, TlsAcceptor::from(tls).accept(io));
        let Some(Ok(Ok(stream))) = shutdown.run_until_cancelled(accept).await else {
            info!(client = %client.id, host = %destination.host, "forward proxy interception handshake failed");
            return;
        };
        let close = CancellationToken::new();
        let connection = Arc::new(Intercepted {
            interceptor: Arc::clone(self),
            host: destination.host.clone(),
            destination: destination.clone(),
            client: Client { id: client.id.clone(), key_hash: client.key_hash.clone() },
            upstream: HttpClient::builder(TokioExecutor::new()).build(Connector {
                dialer: self.dialer.clone(),
                destination: destination.clone(),
                tls: self.upstream_tls.clone(),
            }),
            close: close.clone(),
            shutdown: shutdown.clone(),
        });
        let service = hyper::service::service_fn(move |request| {
            let connection = Arc::clone(&connection);
            let span = tracing::info_span!("forward proxy request", host = %connection.host, client = %connection.client.id);
            async move { Ok::<_, Infallible>(connection.handle(request).await) }.instrument(span)
        });
        let builder = auto::Builder::new(TokioExecutor::new());
        let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        tokio::pin!(conn);
        let revoked = tunnel::wait_for_revocation(&self.keys, &client.key_hash);
        tokio::pin!(revoked);
        let drain = tokio::select! {
            _ = conn.as_mut() => false,
            _ = &mut revoked => false,
            _ = close.cancelled() => true,
            _ = shutdown.cancelled() => true,
        };
        if drain {
            // Take no new request, and give those in flight a moment to finish.
            conn.as_mut().graceful_shutdown();
            let _ = timeout(DRAIN, conn.as_mut()).await;
        }
        info!(
            client = %client.id,
            host = %destination.host,
            received = counts.read.load(Ordering::Relaxed),
            sent = counts.written.load(Ordering::Relaxed),
            duration_ms = started.elapsed().as_millis() as u64,
            "forward proxy interception closed"
        );
    }
}

/// One intercepted client connection.
struct Intercepted {
    interceptor: Arc<Interceptor>,
    host: String,
    destination: Destination,
    client: Client,
    upstream: HttpClient<Connector, ForwardBody>,
    /// Stops the connection after the current response.
    close: CancellationToken,
    shutdown: CancellationToken,
}

impl Intercepted {
    async fn handle(&self, request: Request<Incoming>) -> Response<ProxyBody> {
        let interceptor = &self.interceptor;
        let path = request.uri().path().to_owned();
        let upgrade = request.version() == Version::HTTP_11 && request.headers().contains_key(header::UPGRADE);
        let tap = match &interceptor.capture {
            Some(sink) if !upgrade && is_model_request(request.method(), &path) => {
                let admission = Admission {
                    client_id: self.client.id.clone(),
                    host: self.host.clone(),
                    path: path.clone(),
                    trace_id: crate::telemetry::current_trace_id().unwrap_or_default(),
                };
                let tap = Tap::admit(sink.clone(), admission, interceptor.max_decoded, interceptor.requests.clone());
                tap.head(Direction::Request, Head::new(None, request.headers()));
                Some(tap)
            }
            _ => None,
        };
        let answer = |status: StatusCode, grpc: &str, reason: String| {
            if let Some(tap) = &tap {
                tap.finish(status.as_u16(), Some(format!("proxy_error: {reason}")));
            } else {
                self.count(status.as_u16());
            }
            google_error(status, grpc, &reason)
        };

        if interceptor.keys.lookup(&self.client.key_hash).and_then(allowed_client).is_none() {
            self.close.cancel();
            let mut response = answer(StatusCode::FORBIDDEN, "PERMISSION_DENIED", "the client key no longer has the egress scope".into());
            if request.version() != Version::HTTP_2 {
                response.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            return response;
        }
        if !names_host(&request, &self.host) {
            return answer(StatusCode::MISDIRECTED_REQUEST, "INVALID_ARGUMENT", format!("this connection serves only {}", self.host));
        }
        if upgrade {
            return self.upgrade(request).await;
        }

        let path_and_query = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
        let uri: Uri = match format!("https://{}{}", self.host, path_and_query).parse() {
            Ok(uri) => uri,
            Err(_) => return answer(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", "the request target is not valid".into()),
        };
        let (mut parts, body) = request.into_parts();
        strip_connection_fields(&mut parts.headers, false);
        // `names_host` checked that `Host` names the CONNECT host, which the URI
        // now carries. Hyper sends it as `:authority` over HTTP/2 and as `Host`
        // over HTTP/1.1. Forwarding an HTTP/1.1 client's `Host` to an HTTP/2
        // upstream would put it beside `:authority`, and Google resets that.
        parts.headers.remove(header::HOST);
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        let outgoing = Request::from_parts(parts, ForwardBody::new(body, tap.clone(), Direction::Request, None));

        let idle = interceptor.idle_timeout;
        let response = match timeout(idle, self.upstream.request(outgoing)).await {
            Err(_) => {
                return answer(
                    StatusCode::GATEWAY_TIMEOUT,
                    "DEADLINE_EXCEEDED",
                    format!("the upstream sent no response headers within {} s", idle.as_secs()),
                );
            }
            Ok(Err(error)) if error.is_connect() => {
                return answer(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("can't reach the upstream: {}", chain(&error)));
            }
            Ok(Err(_)) => return answer(StatusCode::BAD_GATEWAY, "UNAVAILABLE", "upstream closed before response headers".into()),
            Ok(Ok(response)) => response,
        };
        let (mut parts, body) = response.into_parts();
        strip_connection_fields(&mut parts.headers, true);
        match &tap {
            Some(tap) => tap.head(Direction::Response, Head::new(Some(parts.status.as_u16()), &parts.headers)),
            None => self.count(parts.status.as_u16()),
        }
        let body = ForwardBody::new(body, tap, Direction::Response, Some(idle));
        Response::from_parts(parts, body.boxed_unsync())
    }

    fn count(&self, status: u16) {
        self.interceptor.requests.add(
            1,
            &[KeyValue::new("host", self.host.clone()), KeyValue::new("status", i64::from(status)), KeyValue::new("capture", "none")],
        );
    }

    /// An HTTP/1.1 upgrade: forwarded on an upstream connection of its own,
    /// then, after a `101`, joined to the client as a tunnel.
    async fn upgrade(&self, mut request: Request<Incoming>) -> Response<ProxyBody> {
        let interceptor = &self.interceptor;
        let failed = |reason: String| {
            self.count(StatusCode::BAD_GATEWAY.as_u16());
            google_error(StatusCode::BAD_GATEWAY, "UNAVAILABLE", &reason)
        };
        let client_upgrade = hyper::upgrade::on(&mut request);
        let connector =
            Connector { dialer: interceptor.dialer.clone(), destination: self.destination.clone(), tls: interceptor.upgrade_tls.clone() };
        let stream = match connector.open().await {
            Ok(stream) => stream,
            Err(error) => return failed(format!("can't reach the upstream: {error}")),
        };
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
            Ok(pair) => pair,
            Err(error) => return failed(format!("can't reach the upstream: {error}")),
        };
        tokio::spawn(conn.with_upgrades());
        let (mut parts, body) = request.into_parts();
        let upgrade = parts.headers.get(header::UPGRADE).cloned();
        strip_connection_fields(&mut parts.headers, false);
        if let Some(upgrade) = upgrade {
            parts.headers.insert(header::UPGRADE, upgrade);
            parts.headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        }
        if !parts.headers.contains_key(header::HOST) {
            parts.headers.insert(header::HOST, HeaderValue::from_str(&self.host).expect("a checked host name"));
        }
        parts.uri = parts.uri.path_and_query().map_or_else(|| Uri::from_static("/"), |pq| pq.as_str().parse().unwrap_or_default());
        let outgoing = Request::from_parts(parts, ForwardBody::new(body, None, Direction::Request, None));
        let mut response = match timeout(interceptor.idle_timeout, sender.send_request(outgoing)).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return failed("upstream closed before response headers".into()),
            Err(_) => {
                self.count(StatusCode::GATEWAY_TIMEOUT.as_u16());
                return google_error(StatusCode::GATEWAY_TIMEOUT, "DEADLINE_EXCEEDED", "the upstream sent no response headers in time");
            }
        };
        self.count(response.status().as_u16());
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (mut parts, body) = response.into_parts();
            strip_connection_fields(&mut parts.headers, true);
            return Response::from_parts(
                parts,
                ForwardBody::new(body, None, Direction::Response, Some(interceptor.idle_timeout)).boxed_unsync(),
            );
        }
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        // The origin's 101, with its application headers, such as
        // `sec-websocket-accept`; only the fields of the switch itself are reset.
        let (mut parts, _) = response.into_parts();
        let selected = parts.headers.get(header::UPGRADE).cloned();
        strip_connection_fields(&mut parts.headers, true);
        if let Some(selected) = selected {
            parts.headers.insert(header::UPGRADE, selected);
        }
        parts.headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        let keys = interceptor.keys.clone();
        let client = Client { id: self.client.id.clone(), key_hash: self.client.key_hash.clone() };
        let idle = interceptor.idle_timeout;
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let (Ok(client_io), Ok(upstream_io)) = tokio::join!(client_upgrade, upstream_upgrade) else { return };
            tunnel::run(TokioIo::new(client_io), TokioIo::new(upstream_io), &[], &client, &keys, idle, &shutdown).await;
        });
        Response::from_parts(parts, Full::new(Bytes::new()).map_err(|never| match never {}).boxed_unsync())
    }
}

/// A Google API error, as the upstream would shape it.
fn google_error(status: StatusCode, grpc: &str, message: &str) -> Response<ProxyBody> {
    let body = json!({"error": {"code": status.as_u16(), "message": message, "status": grpc}}).to_string();
    let mut response = Response::new(Full::new(Bytes::from(body)).map_err(|never| match never {}).boxed_unsync());
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=UTF-8"));
    response
}

/// Whether the request's `Host` and `:authority` name `host`, on port 443 or none.
fn names_host<B>(request: &Request<B>, host: &str) -> bool {
    // A capture host is a DNS name, so an IP literal never matches it.
    let names = |authority: &str| {
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (authority, None),
        };
        let name = name.strip_suffix('.').unwrap_or(name);
        name.eq_ignore_ascii_case(host) && port.is_none_or(|port| port == "443")
    };
    let authority = request.uri().authority().map(|a| a.as_str());
    let host_header = request.headers().get(header::HOST).map(|v| v.to_str().unwrap_or(""));
    (authority.is_some() || host_header.is_some()) && authority.is_none_or(names) && host_header.is_none_or(names)
}

/// Remove the connection-specific fields of RFC 9110, section 7.6.1, and,
/// on a response, `alt-svc`.
fn strip_connection_fields(headers: &mut HeaderMap, response: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in
        ["connection", "keep-alive", "proxy-authorization", "proxy-authenticate", "proxy-connection", "transfer-encoding", "upgrade"]
    {
        headers.remove(name);
    }
    let trailers = headers.get_all(header::TE).iter().any(|v| v.to_str().is_ok_and(|v| v.trim().eq_ignore_ascii_case("trailers")));
    headers.remove(header::TE);
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    if response {
        headers.remove(header::ALT_SVC);
    }
}

fn chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        let next_text = next.to_string();
        if !text.contains(&next_text) {
            text = format!("{text}: {next_text}");
        }
        source = next.source();
    }
    text
}

/// A body forwarded frame by frame, as it arrives, with each data frame
/// handed to the capture.
struct ForwardBody {
    inner: Incoming,
    tap: Option<Tap>,
    direction: Direction,
    /// The longest wait for a frame, and its timer.
    idle: Option<(Duration, Pin<Box<Sleep>>)>,
    ended: bool,
}

impl ForwardBody {
    fn new(inner: Incoming, tap: Option<Tap>, direction: Direction, idle: Option<Duration>) -> Self {
        let idle = idle.map(|idle| (idle, Box::pin(tokio::time::sleep(idle))));
        Self { inner, tap, direction, idle, ended: false }
    }

    fn end(&mut self, error: Option<String>) {
        self.ended = true;
        if let Some(tap) = &self.tap {
            tap.end(self.direction);
            if self.direction == Direction::Response {
                tap.finish(tap.response_status().unwrap_or(502), error);
            }
        }
    }
}

impl Body for ForwardBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some((idle, timer)) = &mut this.idle {
                    timer.as_mut().reset(Instant::now() + *idle);
                }
                if let (Some(tap), Some(data)) = (&this.tap, frame.data_ref()) {
                    tap.frame(this.direction, data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.end(Some(format!("proxy_error: upstream read failed: {}", chain(&error))));
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Ready(None) => {
                this.end(None);
                Poll::Ready(None)
            }
            Poll::Pending => {
                if let Some((idle, timer)) = &mut this.idle {
                    ready!(timer.as_mut().poll(cx));
                    let reason = format!("no data from the upstream for {} s", idle.as_secs());
                    this.end(Some(format!("proxy_error: {reason}")));
                    return Poll::Ready(Some(Err(reason.into())));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ForwardBody {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        if self.inner.is_end_stream() {
            self.end(None);
        } else if self.direction == Direction::Response
            && let Some(tap) = &self.tap
        {
            tap.end(Direction::Response);
            tap.finish(
                tap.response_status().unwrap_or(499),
                Some("proxy_error: the connection closed before the response ended".to_owned()),
            );
        }
    }
}

/// Opens upstream connections to the CONNECT host, whatever the request URI
/// says: resolved, checked, dialed and verified.
#[derive(Clone)]
struct Connector {
    dialer: Arc<dyn Dialer>,
    destination: Destination,
    tls: Arc<ClientConfig>,
}

impl Connector {
    async fn open(&self) -> Result<client::TlsStream<TcpStream>, BoxError> {
        let addresses = timeout(PHASE_TIMEOUT, self.destination.checked_addresses(self.dialer.as_ref()))
            .await
            .map_err(|_| "resolving the host timed out")?
            .map_err(|refused| format!("refused: {}", refused.reason()))?;
        let tcp = timeout(PHASE_TIMEOUT, self.dialer.connect(&addresses)).await.map_err(|_| "connecting timed out")??;
        let name = ServerName::try_from(self.destination.host.clone())?;
        let tls = timeout(PHASE_TIMEOUT, TlsConnector::from(self.tls.clone()).connect(name, tcp))
            .await
            .map_err(|_| "the TLS handshake timed out")?;
        Ok(tls.map_err(|error| format!("TLS: {error}"))?)
    }
}

impl tower::Service<Uri> for Connector {
    type Response = Upstream;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Upstream, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move { Ok(Upstream(TokioIo::new(connector.open().await?))) })
    }
}

/// An upstream TLS connection, for the HTTP client.
struct Upstream(TokioIo<client::TlsStream<TcpStream>>);

impl Connection for Upstream {
    fn connected(&self) -> Connected {
        let h2 = self.0.inner().get_ref().1.alpn_protocol() == Some(b"h2");
        if h2 { Connected::new().negotiated_h2() } else { Connected::new() }
    }
}

impl hyper::rt::Read for Upstream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for Upstream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

#[derive(Default)]
struct Counts {
    read: AtomicU64,
    written: AtomicU64,
}

/// The client's stream, read first from what arrived with the CONNECT head,
/// counting the bytes each way.
struct Counted<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
    counts: Arc<Counts>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.offset < this.prefix.len() {
            let len = (this.prefix.len() - this.offset).min(buf.remaining());
            buf.put_slice(&this.prefix[this.offset..this.offset + len]);
            this.offset += len;
            this.counts.read.fetch_add(len as u64, Ordering::Relaxed);
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        this.counts.read.fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let len = ready!(Pin::new(&mut self.inner).poll_write(cx, buf))?;
        self.counts.written.fetch_add(len as u64, Ordering::Relaxed);
        Poll::Ready(Ok(len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
