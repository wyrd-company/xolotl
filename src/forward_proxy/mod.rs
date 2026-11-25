//! The optional TLS listener for authenticated HTTP/1.1 CONNECT tunnels.

mod certs;
mod destination;
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

pub(crate) use self::certs::Signed;
use self::destination::{Destination, Dialer, TcpDialer};
use crate::config::{ForwardProxyConfig, ForwardProxyTlsConfig};
use crate::keys::{KeyManager, Principal, Scope, sha256_hex};

const PHASE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEAD: usize = 16 * 1024;

pub(crate) struct Listener {
    tcp: TcpListener,
    tls: TlsAcceptor,
    expires_at: i64,
    ports: Vec<u16>,
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
    pub(crate) async fn bind(config: Option<&ForwardProxyConfig>) -> Result<Option<Self>> {
        let Some(config) = config else {
            return Ok(None);
        };
        let (tls, expires_at) = load_tls(&config.tls)?;
        let tcp = TcpListener::bind(config.listen).await.with_context(|| format!("binding forward_proxy.listen {}", config.listen))?;
        Ok(Some(Self { tcp, tls: TlsAcceptor::from(Arc::new(tls)), expires_at, ports: config.ports.clone() }))
    }

    pub(crate) async fn run(self, keys: Arc<KeyManager>, idle_timeout: Duration, shutdown: CancellationToken) -> Result<()> {
        self.run_with(keys, idle_timeout, shutdown, Arc::new(TcpDialer)).await
    }

    async fn run_with(
        self,
        keys: Arc<KeyManager>,
        idle_timeout: Duration,
        shutdown: CancellationToken,
        dialer: Arc<dyn Dialer>,
    ) -> Result<()> {
        let address = self.tcp.local_addr()?;
        info!(%address, "forward proxy listening");
        let mut daily = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        let connections = opentelemetry::global::meter("xolotl").u64_counter("xolotl.forward_proxy.connections").build();
        let context = Arc::new(ConnectionContext {
            tls: self.tls,
            ports: self.ports,
            idle_timeout,
            keys,
            dialer,
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
        Ok(())
    }
}

struct ConnectionContext {
    tls: TlsAcceptor,
    ports: Vec<u16>,
    idle_timeout: Duration,
    keys: Arc<KeyManager>,
    dialer: Arc<dyn Dialer>,
    connections: Counter<u64>,
    shutdown: CancellationToken,
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
        let (upstream, buffered, client) = prepared;
        tunnel::run(stream, upstream, &buffered, &client, &self.keys, self.idle_timeout, &self.shutdown).await;
    }

    async fn prepare(&self, stream: &mut TlsStream<TcpStream>) -> Result<(TcpStream, Vec<u8>, Client), Refusal> {
        let request = timeout(PHASE_TIMEOUT, read_connect(stream)).await.unwrap_or(Err(Refusal::HeadTimeout));
        let (destination, password, buffered) = request.inspect_err(|refused| self.record(None, None, Some(*refused)))?;
        let client = self.authorize(password).await.inspect_err(|refused| self.record(Some(&destination), None, Some(*refused)))?;
        let result = self.connect(&destination).await;
        self.record(Some(&destination), Some(&client.id), result.as_ref().err().copied());
        Ok((result?, buffered, client))
    }

    async fn authorize(&self, password: Option<String>) -> Result<Client, Refusal> {
        let password = password.ok_or(Refusal::Credentials)?;
        let principal = self.keys.authenticate(&password).await.ok_or(Refusal::Credentials)?;
        let id = allowed_client(principal).ok_or(Refusal::Scope)?;
        Ok(Client { id, key_hash: sha256_hex(password.trim()) })
    }

    async fn connect(&self, destination: &Destination) -> Result<TcpStream, Refusal> {
        if !self.ports.contains(&destination.port) {
            return Err(Refusal::Port);
        }
        let addresses =
            timeout(PHASE_TIMEOUT, destination.checked_addresses(self.dialer.as_ref())).await.map_err(|_| Refusal::Resolve)??;
        timeout(PHASE_TIMEOUT, self.dialer.connect(&addresses)).await.map_err(|_| Refusal::Connect)?.map_err(|_| Refusal::Connect)
    }

    fn record(&self, destination: Option<&Destination>, client: Option<&str>, refusal: Option<Refusal>) {
        let outcome = if refusal.is_some() { "refused" } else { "tunnel" };
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
