//! The optional TLS listener for authenticated HTTP/1.1 CONNECT requests:
//! a tunnel to every destination, except the capture hosts, which are
//! intercepted ([`intercept`]) and their model requests captured ([`capture`]).

pub(crate) mod capture;
mod certs;
mod destination;
mod intercept;
mod tunnel;

use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use chrono::Utc;
use opentelemetry::{KeyValue, metrics::Counter};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use self::capture::CaptureSink;
pub(crate) use self::certs::Signed;
use self::certs::{PkiSigner, Store};
use self::destination::{Destination, Dialer, TcpDialer};
use self::intercept::Interceptor;
use crate::config::{ForwardProxyConfig, ForwardProxyTlsConfig};
use crate::keys::{KeyManager, Principal, Scope, sha256_hex};
use crate::vault::VaultClient;
use tokio_rustls::rustls::client::danger::ServerCertVerifier;

const PHASE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEAD: usize = 16 * 1024;

pub(crate) struct Listener {
    tcp: TcpListener,
    tls: TlsAcceptor,
    expires_at: i64,
    ports: Vec<u16>,
    /// The capture hosts' certificates, when there are capture hosts.
    certs: Option<Arc<Store>>,
}

/// What the listener needs from the rest of xolotl.
pub(crate) struct Services {
    pub keys: Arc<KeyManager>,
    /// `server.upstream_timeout_secs`: the longest a tunnel idles, and an
    /// intercepted request waits for the upstream.
    pub idle_timeout: Duration,
    /// `server.max_body_bytes`: the most of a body the capture reads.
    pub max_body_bytes: usize,
    /// Receives the model requests on capture hosts.
    pub capture: Option<Arc<dyn CaptureSink>>,
}

pub(crate) fn load_tls(config: &ForwardProxyTlsConfig) -> Result<(ServerConfig, i64)> {
    let certs = CertificateDer::pem_file_iter(&config.cert_file)
        .with_context(|| format!("reading forward_proxy.tls.cert_file {}", config.cert_file.display()))?
        .collect::<Result<Vec<_>, _>>()
        .context("parsing forward_proxy.tls.cert_file as PEM")?;
    anyhow::ensure!(!certs.is_empty(), "forward_proxy.tls.cert_file has no certificates");
    let mut expires_at = i64::MAX;
    for der in &certs {
        let (remaining, cert) = x509_parser::parse_x509_certificate(der).context("parsing forward_proxy.tls.cert_file as X.509")?;
        anyhow::ensure!(remaining.is_empty(), "forward_proxy.tls.cert_file contains invalid certificate data");
        let validity = cert.validity();
        anyhow::ensure!(validity.is_valid(), "forward_proxy.tls.cert_file contains an expired or not-yet-valid certificate");
        expires_at = expires_at.min(validity.not_after.timestamp());
    }
    let key = PrivateKeyDer::from_pem_file(&config.key_file)
        .with_context(|| format!("reading forward_proxy.tls.key_file {}", config.key_file.display()))?;
    let mut tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("forward_proxy.tls certificate and private key must match")?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((tls, expires_at))
}

impl Listener {
    pub(crate) async fn bind(config: Option<&ForwardProxyConfig>, vault: Option<&VaultClient>) -> Result<Option<Self>> {
        let Some(config) = config else {
            return Ok(None);
        };
        let (tls, expires_at) = load_tls(&config.tls)?;
        let certs = match (&config.pki, config.capture_hosts.is_empty()) {
            (_, true) => None,
            (Some(pki), false) => {
                let vault = vault.context("forward_proxy.capture_hosts needs the vault")?.clone();
                let signer = PkiSigner { vault, mount: pki.mount.clone(), role: pki.role.clone() };
                Some(Store::new(&config.capture_hosts, Arc::new(signer)))
            }
            (None, false) => anyhow::bail!("forward_proxy.capture_hosts needs [forward_proxy.pki]"),
        };
        let tcp = TcpListener::bind(config.listen).await.with_context(|| format!("binding forward_proxy.listen {}", config.listen))?;
        Ok(Some(Self { tcp, tls: TlsAcceptor::from(Arc::new(tls)), expires_at, ports: config.ports.clone(), certs }))
    }

    pub(crate) async fn run(self, services: Services, shutdown: CancellationToken) -> Result<()> {
        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new(provider).context("loading the system roots for upstream TLS")?;
        self.run_with(services, shutdown, Arc::new(TcpDialer), Arc::new(verifier)).await
    }

    async fn run_with(
        self,
        services: Services,
        shutdown: CancellationToken,
        dialer: Arc<dyn Dialer>,
        verifier: Arc<dyn ServerCertVerifier>,
    ) -> Result<()> {
        let address = self.tcp.local_addr()?;
        info!(%address, "forward proxy listening");
        let mut daily = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        let connections = opentelemetry::global::meter("xolotl").u64_counter("xolotl.forward_proxy.connections").build();
        let certificates = self.certs.clone().map(|store| tokio::spawn(store.run(shutdown.clone())));
        let Services { keys, idle_timeout, max_body_bytes, capture } = services;
        let interceptor = Arc::new(Interceptor::new(keys.clone(), dialer.clone(), verifier, idle_timeout, capture, max_body_bytes));
        let context = Arc::new(ConnectionContext {
            tls: self.tls,
            ports: self.ports,
            idle_timeout,
            keys,
            dialer,
            certs: self.certs,
            interceptor,
            connections,
            shutdown: shutdown.clone(),
        });
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = daily.tick() => {
                    if self.expires_at - Utc::now().timestamp() <= 14 * 24 * 60 * 60 {
                        warn!(expires_at = %chrono::DateTime::from_timestamp(self.expires_at, 0).unwrap_or_default(),
                              "forward proxy listener certificate expires within 14 days");
                    }
                }
                Some(_) = tasks.join_next() => {}
                accepted = self.tcp.accept() => match accepted {
                    Ok((stream, _)) => {
                        let context = Arc::clone(&context);
                        tasks.spawn(async move { context.serve(stream).await });
                    }
                    Err(error) if matches!(error.kind(), ErrorKind::ConnectionRefused | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset) => {}
                    Err(error) => {
                        warn!(%error, "forward proxy accept failed");
                        let _ = shutdown.run_until_cancelled(tokio::time::sleep(Duration::from_secs(1))).await;
                    }
                },
            }
        }
        drop(self.tcp);
        while tasks.join_next().await.is_some() {}
        if let Some(certificates) = certificates {
            let _ = certificates.await;
        }
        Ok(())
    }
}

struct ConnectionContext {
    tls: TlsAcceptor,
    ports: Vec<u16>,
    idle_timeout: Duration,
    keys: Arc<KeyManager>,
    dialer: Arc<dyn Dialer>,
    certs: Option<Arc<Store>>,
    interceptor: Arc<Interceptor>,
    connections: Counter<u64>,
    shutdown: CancellationToken,
}

/// What a CONNECT that passed the checks becomes.
enum Route {
    Tunnel(TcpStream),
    Intercept(Arc<ServerConfig>),
}

struct Client {
    id: String,
    key_hash: String,
}

fn allowed_client(principal: Principal) -> Option<String> {
    principal.has_scope(Scope::Egress).then_some(principal.client_id).flatten()
}

impl ConnectionContext {
    async fn serve(&self, stream: TcpStream) {
        let Some(Ok(Ok(mut stream))) = self.shutdown.run_until_cancelled(timeout(PHASE_TIMEOUT, self.tls.accept(stream))).await else {
            return;
        };
        let Some(prepared) = self
            .shutdown
            .run_until_cancelled(async {
                match self.prepare(&mut stream).await {
                    Ok(prepared) => {
                        timeout(PHASE_TIMEOUT, async {
                            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
                            stream.flush().await
                        })
                        .await
                        .ok()?
                        .ok()?;
                        Some(prepared)
                    }
                    Err(refused) => {
                        let _ = timeout(PHASE_TIMEOUT, async {
                            stream.write_all(refused.response()).await?;
                            stream.shutdown().await
                        })
                        .await;
                        None
                    }
                }
            })
            .await
            .flatten()
        else {
            return;
        };
        let (route, buffered, client, destination) = prepared;
        match route {
            Route::Tunnel(upstream) => {
                tunnel::run(stream, upstream, &buffered, &client, &self.keys, self.idle_timeout, &self.shutdown).await;
            }
            Route::Intercept(tls) => self.interceptor.serve(stream, buffered, tls, &destination, &client, &self.shutdown).await,
        }
    }

    async fn prepare(&self, stream: &mut TlsStream<TcpStream>) -> Result<(Route, Vec<u8>, Client, Destination), Refusal> {
        let request = timeout(PHASE_TIMEOUT, read_connect(stream)).await.unwrap_or(Err(Refusal::HeadTimeout));
        let (destination, password, buffered) = request.inspect_err(|refused| self.record(None, None, Err(*refused)))?;
        let client = self.authorize(password).await.inspect_err(|refused| self.record(Some(&destination), None, Err(*refused)))?;
        let result = self.route(&destination).await;
        self.record(Some(&destination), Some(&client.id), result.as_ref().map(|(_, outcome)| *outcome).map_err(|refused| *refused));
        Ok((result?.0, buffered, client, destination))
    }

    async fn authorize(&self, password: Option<String>) -> Result<Client, Refusal> {
        let password = password.ok_or(Refusal::Credentials)?;
        let principal = self.keys.authenticate(&password).await.ok_or(Refusal::Credentials)?;
        let id = allowed_client(principal).ok_or(Refusal::Scope)?;
        Ok(Client { id, key_hash: sha256_hex(password.trim()) })
    }

    /// Check the destination, then intercept a capture host that has a
    /// certificate, and tunnel to anything else.
    async fn route(&self, destination: &Destination) -> Result<(Route, &'static str), Refusal> {
        if !self.ports.contains(&destination.port) {
            return Err(Refusal::Port);
        }
        let addresses =
            timeout(PHASE_TIMEOUT, destination.checked_addresses(self.dialer.as_ref())).await.map_err(|_| Refusal::Resolve)??;
        let mut outcome = "tunnel";
        if let Some(certs) = &self.certs
            && destination.port == 443
            && certs.is_capture_host(&destination.host)
        {
            if let Some(tls) = certs.get(&destination.host).await {
                return Ok((Route::Intercept(tls), "intercept"));
            }
            warn!(host = %destination.host, "forward proxy has no valid certificate for the capture host; tunneling it");
            outcome = "fallback";
        }
        let upstream =
            timeout(PHASE_TIMEOUT, self.dialer.connect(&addresses)).await.map_err(|_| Refusal::Connect)?.map_err(|_| Refusal::Connect)?;
        Ok((Route::Tunnel(upstream), outcome))
    }

    fn record(&self, destination: Option<&Destination>, client: Option<&str>, outcome: Result<&'static str, Refusal>) {
        let refusal = outcome.err();
        let outcome = outcome.unwrap_or("refused");
        let mut attributes = vec![KeyValue::new("outcome", outcome)];
        if let Some(client) = client {
            attributes.push(KeyValue::new("client", client.to_owned()));
        }
        if let Some(refused) = refusal {
            attributes.push(KeyValue::new("reason", refused.reason()));
        }
        self.connections.add(1, &attributes);
        info!(
            client,
            host = destination.map(|d| d.host.as_str()),
            port = destination.map(|d| d.port),
            outcome,
            reason = refusal.map(Refusal::reason),
            "forward proxy CONNECT"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum Refusal {
    Method,
    Authority,
    HeadTimeout,
    HeadTooLarge,
    Credentials,
    Scope,
    Port,
    Resolve,
    Address,
    Connect,
}

impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Authority => "invalid_request",
            Self::HeadTimeout => "head_timeout",
            Self::HeadTooLarge => "head_too_large",
            Self::Credentials => "credentials",
            Self::Scope => "scope",
            Self::Port => "port",
            Self::Resolve => "resolve",
            Self::Address => "address",
            Self::Connect => "connect",
        }
    }

    fn response(self) -> &'static [u8] {
        match self {
            Self::Method => b"HTTP/1.1 405 Method Not Allowed\r\nAllow: CONNECT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::Authority => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::HeadTimeout => b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::HeadTooLarge => b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::Credentials => b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"xolotl\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::Scope | Self::Port | Self::Address => b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Self::Resolve | Self::Connect => b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        }
    }
}

async fn read_connect(stream: &mut (impl AsyncRead + Unpin)) -> Result<(Destination, Option<String>, Vec<u8>), Refusal> {
    let mut head = Vec::new();
    let mut buffer = [0; 2048];
    let end = loop {
        let available = (MAX_HEAD - head.len()).min(buffer.len());
        if available == 0 {
            return Err(Refusal::HeadTooLarge);
        }
        let len = stream.read(&mut buffer[..available]).await.map_err(|_| Refusal::Authority)?;
        if len == 0 {
            return Err(Refusal::Authority);
        }
        let scan_from = head.len().saturating_sub(3);
        head.extend_from_slice(&buffer[..len]);
        if let Some(offset) = head[scan_from..].windows(4).position(|chunk| chunk == b"\r\n\r\n") {
            break scan_from + offset + 4;
        }
    };
    let request = &head[..end];
    let header_lines = request.iter().filter(|byte| **byte == b'\n').count() - 2;
    let mut headers = vec![httparse::EMPTY_HEADER; header_lines];
    let mut parsed = httparse::Request::new(&mut headers);
    if parsed.parse(request) != Ok(httparse::Status::Complete(end)) {
        return Err(Refusal::Authority);
    }
    if parsed.method != Some("CONNECT") {
        return Err(Refusal::Method);
    }
    if parsed.version != Some(1) {
        return Err(Refusal::Authority);
    }
    let destination = parsed.path.and_then(Destination::parse).ok_or(Refusal::Authority)?;
    let password = proxy_password(parsed.headers);
    Ok((destination, password, head.split_off(end)))
}

fn proxy_password(headers: &[httparse::Header<'_>]) -> Option<String> {
    let mut auth = headers.iter().filter(|header| header.name.eq_ignore_ascii_case("proxy-authorization"));
    let header = auth.next()?;
    if auth.next().is_some() {
        return None;
    }
    let mut parts = std::str::from_utf8(header.value).ok()?.split_ascii_whitespace();
    let scheme = parts.next()?;
    let encoded = parts.next()?;
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let auth = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded).ok()?).ok()?;
    let (_, password) = auth.split_once(':')?;
    Some(password.to_owned())
}

#[cfg(test)]
mod testpki;
#[cfg(test)]
mod tests;
