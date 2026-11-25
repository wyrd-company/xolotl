use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsConnector, client};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;
use crate::config::Config;
use crate::export::{Exporter, Sink};
use crate::policy::Policy;
use crate::server::tests::harness;

const ORIGIN: &str = "origin.example";
const PUBLIC: &str = "8.8.8.8:443";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_ENV: &str = "XOLOTL_TUNNEL_TEST_TRUST";
const CLIENT_TEST: &str = "forward_proxy::tests::https_proxy_tunnels_without_an_exchange";

struct TestTls {
    dir: tempfile::TempDir,
    config: ForwardProxyTlsConfig,
    cert: CertificateDer<'static>,
}

impl TestTls {
    fn new(hostname: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = ForwardProxyTlsConfig { cert_file: dir.path().join("cert.pem"), key_file: dir.path().join("key.pem") };
        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(vec![hostname.to_owned()]).unwrap();
        std::fs::write(&config.cert_file, cert.pem()).unwrap();
        std::fs::write(&config.key_file, signing_key.serialize_pem()).unwrap();
        Self { dir, config, cert: cert.der().clone() }
    }
}

#[derive(Default)]
struct TestDialer {
    upstream: Option<SocketAddr>,
    resolutions: Mutex<Vec<(String, u16)>>,
    connections: Mutex<Vec<Vec<SocketAddr>>>,
}

#[async_trait::async_trait]
impl Dialer for TestDialer {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.resolutions.lock().unwrap().push((host.to_owned(), port));
        match host {
            ORIGIN => Ok(vec![PUBLIC.parse().unwrap()]),
            "mixed.example" => Ok(vec![PUBLIC.parse().unwrap(), "10.0.0.1:443".parse().unwrap()]),
            "missing.example" => Err(ErrorKind::NotFound.into()),
            _ => panic!("unexpected resolution for {host}:{port}"),
        }
    }

    async fn connect(&self, addresses: &[SocketAddr]) -> io::Result<TcpStream> {
        self.connections.lock().unwrap().push(addresses.to_vec());
        let upstream = self.upstream.ok_or(ErrorKind::ConnectionRefused)?;
        TcpStream::connect(upstream).await
    }
}

struct Proxy {
    address: SocketAddr,
    tls: TestTls,
    shutdown: CancellationToken,
    task: JoinHandle<Result<()>>,
}

impl Proxy {
    async fn start(keys: Arc<KeyManager>, dialer: Arc<TestDialer>, idle: Duration) -> Self {
        let tls = TestTls::new("127.0.0.1");
        let config = ForwardProxyConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ports: vec![443],
            capture_hosts: vec![],
            tls: tls.config.clone(),
            pki: None,
        };
        let listener = Listener::bind(Some(&config)).await.unwrap().unwrap();
        let address = listener.tcp.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(listener.run_with(keys, idle, shutdown.clone(), dialer));
        Self { address, tls, shutdown, task }
    }

    async fn stream(&self) -> client::TlsStream<TcpStream> {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.add(self.tls.cert.clone()).unwrap();
        let tls = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        TlsConnector::from(Arc::new(tls)).connect(self.address.ip().into(), TcpStream::connect(self.address).await.unwrap()).await.unwrap()
    }

    async fn response(&self, request: &str) -> String {
        let mut stream = self.stream().await;
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let mut response = String::new();
        timeout(TEST_TIMEOUT, stream.read_to_string(&mut response)).await.unwrap().unwrap();
        response
    }

    async fn stop(self) {
        self.shutdown.cancel();
        timeout(TEST_TIMEOUT, self.task).await.unwrap().unwrap().unwrap();
    }
}

fn connect_request(authority: &str, key: &str) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("unused:{key}"));
    format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: Basic {basic}\r\n\r\n")
}

fn egress_policy() -> Policy {
    Policy { name: "egress".into(), scopes: vec![Scope::Egress], ..Default::default() }
}

async fn read_response_head<S: AsyncRead + Unpin>(stream: S) -> BufReader<S> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    assert_eq!(line, "HTTP/1.1 200 Connection Established\r\n");
    line.clear();
    stream.read_line(&mut line).await.unwrap();
    assert_eq!(line, "\r\n");
    stream
}

#[tokio::test]
async fn https_proxy_tunnels_without_an_exchange() {
    if let Some(trust) = std::env::var_os(CLIENT_ENV) {
        let certs = reqwest::Certificate::from_pem_bundle(&std::fs::read(trust).unwrap()).unwrap();
        let client = reqwest::Client::builder().timeout(TEST_TIMEOUT).tls_certs_only(certs).build().unwrap();
        let response = client
            .get(format!("https://{ORIGIN}/model"))
            .header("authorization", "Bearer origin-fixture")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "passed");
        return;
    }

    let mut h = harness().await;
    let (rows, mut records) = tokio::sync::mpsc::unbounded_channel();
    let sink = Sink::spawn("forward-proxy-test", true, 4096, 1, |mut queue| async move {
        loop {
            tokio::select! {
                Some(row) = queue.rx.recv() => {
                    queue.done(&row);
                    rows.send(row).unwrap();
                }
                _ = &mut queue.stop => {
                    while let Ok(row) = queue.rx.try_recv() {
                        queue.done(&row);
                        rows.send(row).unwrap();
                    }
                    break;
                }
            }
        }
    });
    let exporter = Exporter::new(vec![sink]);
    h.state.export = Some(exporter.clone());
    let dedicated = h.key(egress_policy()).await;
    let shared = h
        .key(Policy {
            name: "shared".into(),
            providers: vec!["llama".into()],
            scopes: vec![Scope::Proxy, Scope::Egress],
            ..Default::default()
        })
        .await;

    let tls = TestTls::new(ORIGIN);
    let (origin_tls, _) = load_tls(&tls.config).unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(origin_tls));
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dialer = Arc::new(TestDialer { upstream: Some(upstream.local_addr().unwrap()), ..Default::default() });
    let proxy = Proxy::start(h.state.keys.clone(), dialer.clone(), TEST_TIMEOUT).await;
    let origin = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = upstream.accept().await.unwrap();
            let mut stream = BufReader::new(acceptor.accept(stream).await.unwrap());
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
            }
            assert!(request.starts_with("GET /model HTTP/1.1\r\n"));
            assert!(request.contains("\r\nauthorization: Bearer origin-fixture\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            stream.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\npassed").await.unwrap();
            stream.get_mut().shutdown().await.unwrap();
        }
    });

    let trust = proxy.tls.dir.path().join("trust.pem");
    let bundle = [std::fs::read(&proxy.tls.config.cert_file).unwrap(), std::fs::read(&tls.config.cert_file).unwrap()].concat();
    std::fs::write(&trust, bundle).unwrap();
    // A fresh process uses HTTPS_PROXY and avoids changing the test runner's environment.
    for key in [&dedicated, &shared] {
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CLIENT_TEST, "--nocapture"])
            .env_remove("https_proxy")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .env("HTTPS_PROXY", format!("https://client:{key}@{}", proxy.address))
            .env(CLIENT_ENV, &trust)
            .kill_on_drop(true)
            .output();
        let result = timeout(TEST_TIMEOUT * 2, child).await.unwrap().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    }
    timeout(TEST_TIMEOUT, origin).await.unwrap().unwrap();
    proxy.stop().await;
    assert_eq!(*dialer.resolutions.lock().unwrap(), vec![(ORIGIN.into(), 443); 2]);
    assert_eq!(*dialer.connections.lock().unwrap(), vec![vec![PUBLIC.parse::<SocketAddr>().unwrap()]; 2]);
    assert!(h.state.db.request_counts_since(0).await.unwrap().is_empty());
    assert!(matches!(records.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));

    // A normal LLM request with the shared key proves that this exporter receives rows.
    Mock::given(method("POST"))
        .and(path("/llama/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .mount(&h.upstream)
        .await;
    let request = json!({"model": "qwen3", "messages": []});
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&dedicated), Some(request.clone())).await.0, 403);
    assert_eq!(h.call("POST", "/v1/chat/completions", Some(&shared), Some(request)).await.0, 200);
    exporter.shutdown().await;
    let row = records.try_recv().unwrap();
    assert_eq!(row.client_id, "shared/client");
    assert!(records.try_recv().is_err());
}

#[tokio::test]
async fn rejected_requests_stop_before_resolving_or_dialing() {
    let h = harness().await;
    let key = h.key(egress_policy()).await;
    let ordinary = h.key(Policy { name: "ordinary".into(), providers: vec!["llama".into()], ..Default::default() }).await;
    let admin = h.key(Policy { name: "admin".into(), providers: vec!["*".into()], scopes: vec![Scope::Admin], ..Default::default() }).await;
    let dialer = Arc::new(TestDialer::default());
    let proxy = Proxy::start(h.state.keys, dialer.clone(), TEST_TIMEOUT).await;

    let oversized = format!("CONNECT {ORIGIN}:443 HTTP/1.1\r\nX-Test: {}\r\n\r\n", "x".repeat(MAX_HEAD));
    let cases = [
        ("GET / HTTP/1.1\r\n\r\n".into(), 405),
        (format!("CONNECT https://{ORIGIN}:443/ HTTP/1.1\r\n\r\n"), 400),
        (format!("CONNECT {ORIGIN} HTTP/1.1\r\n\r\n"), 400),
        ("CONNECT two..labels:443 HTTP/1.1\r\n\r\n".into(), 400),
        (format!("CONNECT {ORIGIN}:443 HTTP/1.1\r\n\r\n"), 407),
        (connect_request("origin.example:443", "xol_unknown"), 407),
        (connect_request("origin.example:443", &ordinary), 403),
        (connect_request("origin.example:443", &admin), 403),
        (connect_request("origin.example:443", "boot"), 403),
        (connect_request("origin.example:8443", &key), 403),
        (oversized, 431),
    ];
    for (request, status) in cases {
        let response = proxy.response(&request).await;
        assert!(response.starts_with(&format!("HTTP/1.1 {status} ")), "{response}");
        if status == 407 {
            assert!(response.contains("Proxy-Authenticate: Basic realm=\"xolotl\"\r\n"));
        }
    }
    assert!(dialer.resolutions.lock().unwrap().is_empty());
    assert!(dialer.connections.lock().unwrap().is_empty());

    for authority in [
        "10.0.0.1:443",
        "127.0.0.1:443",
        "169.254.1.1:443",
        "100.64.0.1:443",
        "224.0.1.1:443",
        "[::1]:443",
        "[fe80::1]:443",
        "[::ffff:10.0.0.1]:443",
        "[64:ff9b::a00:1]:443",
        "[64:ff9b:1::1]:443",
        "[2001::1]:443",
        "[2002::1]:443",
        "[3fff::1]:443",
        "mixed.example:443",
    ] {
        let response = proxy.response(&connect_request(authority, &key)).await;
        assert!(response.starts_with("HTTP/1.1 403 "), "refusing {authority}: {response}");
    }
    assert!(proxy.response(&connect_request("missing.example:443", &key)).await.starts_with("HTTP/1.1 502 "));
    assert_eq!(*dialer.resolutions.lock().unwrap(), vec![("mixed.example".into(), 443), ("missing.example".into(), 443)]);
    assert!(dialer.connections.lock().unwrap().is_empty());

    assert!(proxy.response(&connect_request("ORIGIN.EXAMPLE.:443", &key)).await.starts_with("HTTP/1.1 502 "));
    assert_eq!(dialer.resolutions.lock().unwrap().last(), Some(&(ORIGIN.into(), 443)));
    assert_eq!(*dialer.connections.lock().unwrap(), vec![vec![PUBLIC.parse::<SocketAddr>().unwrap()]]);
    proxy.stop().await;
}

#[tokio::test]
async fn revoked_key_closes_an_open_tunnel() {
    let h = harness().await;
    let key = h.key(egress_policy()).await;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dialer = Arc::new(TestDialer { upstream: Some(upstream.local_addr().unwrap()), ..Default::default() });
    let proxy = Proxy::start(h.state.keys.clone(), dialer, Duration::from_secs(60)).await;
    let mut stream = proxy.stream().await;
    // The initial bytes can arrive in the same read as the CONNECT head.
    stream.write_all(format!("{}ping", connect_request("origin.example:443", &key)).as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut stream = timeout(TEST_TIMEOUT, read_response_head(stream)).await.unwrap();
    let (mut upstream, _) = timeout(TEST_TIMEOUT, upstream.accept()).await.unwrap().unwrap();
    let mut ping = [0; 4];
    timeout(TEST_TIMEOUT, upstream.read_exact(&mut ping)).await.unwrap().unwrap();
    assert_eq!(&ping, b"ping");
    h.state.keys.revoke("egress/client").await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(15)).await;
    assert_eq!(timeout(TEST_TIMEOUT, stream.read(&mut ping)).await.unwrap().unwrap(), 0);
    assert_eq!(timeout(TEST_TIMEOUT, upstream.read(&mut ping)).await.unwrap().unwrap(), 0);
    proxy.stop().await;
}

#[tokio::test]
async fn idle_and_slow_connections_close() {
    let h = harness().await;
    let key = h.key(egress_policy()).await;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dialer = Arc::new(TestDialer { upstream: Some(upstream.local_addr().unwrap()), ..Default::default() });
    let proxy = Proxy::start(h.state.keys, dialer, Duration::from_millis(200)).await;
    let mut stream = proxy.stream().await;
    stream.write_all(connect_request("origin.example:443", &key).as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut stream = timeout(TEST_TIMEOUT, read_response_head(stream)).await.unwrap();
    let (mut upstream, _) = timeout(TEST_TIMEOUT, upstream.accept()).await.unwrap().unwrap();
    assert_eq!(timeout(TEST_TIMEOUT, stream.read(&mut [0])).await.unwrap().unwrap(), 0);
    assert_eq!(timeout(TEST_TIMEOUT, upstream.read(&mut [0])).await.unwrap().unwrap(), 0);

    let mut slow_head = proxy.stream().await;
    slow_head.write_all(b"CONNECT").await.unwrap();
    slow_head.flush().await.unwrap();
    let mut slow_tls = TcpStream::connect(proxy.address).await.unwrap();
    let mut scratch = [0];
    let (head, handshake) = tokio::join!(
        timeout(PHASE_TIMEOUT + TEST_TIMEOUT, async {
            let mut response = String::new();
            slow_head.read_to_string(&mut response).await.unwrap();
            response
        }),
        timeout(PHASE_TIMEOUT + TEST_TIMEOUT, slow_tls.read(&mut scratch)),
    );
    assert!(head.unwrap().starts_with("HTTP/1.1 408 "));
    assert_eq!(handshake.unwrap().unwrap(), 0);
    proxy.stop().await;
}

#[tokio::test]
async fn disabled_config_does_not_bind() {
    let config: Config = toml::from_str("").unwrap();
    assert!(Listener::bind(config.forward_proxy.as_ref()).await.unwrap().is_none());
    let missing_tls = "[forward_proxy]\nlisten = \"127.0.0.1:8318\"";
    assert!(toml::from_str::<Config>(missing_tls).unwrap_err().to_string().contains("missing field `tls`"));
}

#[test]
fn validates_listen_ports_and_tls_material() {
    let tls = TestTls::new("127.0.0.1");
    let proxy = ForwardProxyConfig {
        listen: "127.0.0.1:8318".parse().unwrap(),
        ports: vec![443],
        capture_hosts: vec![],
        tls: tls.config.clone(),
        pki: None,
    };
    let valid = Config { forward_proxy: Some(proxy.clone()), ..Default::default() };
    valid.validate().unwrap();
    let encoded = toml::to_string(&valid).unwrap().replace("ports = [443]\n", "");
    assert!(!encoded.contains("ports = [443]"));
    assert_eq!(toml::from_str::<Config>(&encoded).unwrap().forward_proxy.unwrap().ports, vec![443]);

    let mut config = valid.clone();
    config.server.listen = proxy.listen;
    assert!(config.validate().unwrap_err().to_string().contains("must differ"));
    for ports in [vec![], vec![0, 443]] {
        config = valid.clone();
        config.forward_proxy.as_mut().unwrap().ports = ports;
        assert!(config.validate().unwrap_err().to_string().contains("forward_proxy.ports"));
    }
    let other = TestTls::new("127.0.0.1");
    config = valid.clone();
    config.forward_proxy.as_mut().unwrap().tls.key_file = other.config.key_file;
    assert!(config.validate().unwrap_err().to_string().contains("must match"));

    config = valid;
    let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
    params.not_before = rcgen::date_time_ymd(1999, 1, 1);
    params.not_after = rcgen::date_time_ymd(2000, 1, 1);
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(&tls.config.cert_file, params.self_signed(&key).unwrap().pem()).unwrap();
    std::fs::write(&tls.config.key_file, key.serialize_pem()).unwrap();
    assert!(config.validate().unwrap_err().to_string().contains("expired or not-yet-valid"));

    config.forward_proxy.as_mut().unwrap().tls.cert_file = tls.dir.path().join("missing.pem");
    assert!(config.validate().unwrap_err().to_string().contains("reading forward_proxy.tls.cert_file"));
}

#[test]
fn validates_capture_hosts_and_pki() {
    let tls = TestTls::new("127.0.0.1");
    let proxy = ForwardProxyConfig {
        listen: "127.0.0.1:8318".parse().unwrap(),
        ports: vec![443],
        capture_hosts: vec!["daily-cloudcode-pa.googleapis.com".into(), "second.example".into()],
        tls: tls.config.clone(),
        pki: Some(crate::config::ForwardProxyPkiConfig { mount: "pki_xolotl_proxy".into(), role: "xolotl-forward-proxy".into() }),
    };
    let mut valid = Config { forward_proxy: Some(proxy), ..Default::default() };
    valid.export.nats = Some(crate::config::NatsConfig { url: "nats://nats:4222".into(), ..Default::default() });
    valid.validate().unwrap();

    for host in [
        "Daily.example",
        "example",
        "*.example.com",
        "example.com:443",
        "example.com.",
        "a..example",
        "10.0.0.1",
        "-a.example",
        "a_b.example",
        "bücher.example",
    ] {
        let mut config = valid.clone();
        config.forward_proxy.as_mut().unwrap().capture_hosts = vec![host.into()];
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("forward_proxy.capture_hosts"), "{host}: {error}");
    }
    let mut config = valid.clone();
    config.forward_proxy.as_mut().unwrap().capture_hosts = vec!["a.example".into(), "a.example".into()];
    assert!(config.validate().unwrap_err().to_string().contains("twice"));

    config = valid.clone();
    config.forward_proxy.as_mut().unwrap().pki = None;
    assert!(config.validate().unwrap_err().to_string().contains("needs [forward_proxy.pki]"));
    config = valid.clone();
    config.forward_proxy.as_mut().unwrap().pki.as_mut().unwrap().role = "../root".into();
    assert!(config.validate().unwrap_err().to_string().contains("forward_proxy.pki"));
    config = valid.clone();
    config.export.nats = None;
    assert!(config.validate().unwrap_err().to_string().contains("needs an export destination"));

    // Without capture hosts, neither PKI nor an export is needed.
    config.forward_proxy.as_mut().unwrap().capture_hosts.clear();
    config.forward_proxy.as_mut().unwrap().pki = None;
    config.validate().unwrap();
    let parsed: Config = toml::from_str(&toml::to_string(&valid).unwrap()).unwrap();
    assert_eq!(parsed.forward_proxy.unwrap().capture_hosts.len(), 2);
}

#[tokio::test]
async fn egress_policy_rejects_administrative_scopes() {
    let h = harness().await;
    let mut policy = egress_policy();
    for scope in [Scope::Admin, Scope::Management, Scope::Keys] {
        policy.scopes = vec![Scope::Egress, scope];
        let result = h.state.keys.put_policy(policy.clone(), &Default::default(), &Default::default()).await;
        assert!(result.unwrap_err().to_string().contains("egress scope cannot be combined"));
    }
    assert!(!Principal::operator().has_scope(Scope::Egress));
    assert!(egress_policy().grants_only_client_use());
}
