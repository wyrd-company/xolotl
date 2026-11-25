//! Certificates for the capture hosts, signed through the OpenBao PKI and held
//! in memory only.
#![cfg_attr(not(test), expect(dead_code, reason = "the interception commit serves these certificates"))]

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use opentelemetry::{KeyValue, metrics::Counter};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, pem::PemObject};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_util::sync::CancellationToken;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::vault::VaultClient;

/// How long a CONNECT waits for an issuance in progress.
pub(super) const ISSUE_WAIT: Duration = Duration::from_secs(10);
const RETRY_MIN: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// A certificate as the PKI returned it: the leaf and its issuers, PEM.
pub(crate) struct Signed {
    pub certificate: String,
    pub ca_chain: Vec<String>,
}

/// Signs a CSR for one capture host.
#[async_trait::async_trait]
pub(crate) trait Signer: Send + Sync {
    async fn sign(&self, csr: &str, common_name: &str) -> Result<Signed>;
}

/// The configured role on an OpenBao PKI mount.
pub(crate) struct PkiSigner {
    pub vault: VaultClient,
    pub mount: String,
    pub role: String,
}

#[async_trait::async_trait]
impl Signer for PkiSigner {
    async fn sign(&self, csr: &str, common_name: &str) -> Result<Signed> {
        self.vault.pki_sign(&self.mount, &self.role, csr, common_name).await
    }
}

/// A certificate in use: the TLS config that serves it, and when it stops
/// being valid.
pub(super) struct Issued {
    pub tls: Arc<ServerConfig>,
    not_before: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
}

impl Issued {
    fn valid(&self) -> bool {
        Utc::now() < self.valid_until
    }

    /// When two thirds of its lifetime have passed.
    fn renew_at(&self) -> DateTime<Utc> {
        self.not_before + (self.valid_until - self.not_before) * 2 / 3
    }
}

#[derive(Clone, Default)]
struct Slot {
    issued: Option<Arc<Issued>>,
    issuing: bool,
}

/// One certificate per capture host.
pub(super) struct Store {
    slots: HashMap<String, watch::Sender<Slot>>,
    signer: Arc<dyn Signer>,
    issued: Counter<u64>,
}

impl Store {
    pub fn new(hosts: &[String], signer: Arc<dyn Signer>) -> Arc<Self> {
        let slots = hosts.iter().map(|host| (host.clone(), watch::Sender::new(Slot { issued: None, issuing: true }))).collect();
        let issued = opentelemetry::global::meter("xolotl").u64_counter("xolotl.forward_proxy.certificates").build();
        Arc::new(Self { slots, signer, issued })
    }

    pub fn is_capture_host(&self, host: &str) -> bool {
        self.slots.contains_key(host)
    }

    /// The TLS config for `host`: the cached certificate, or the one being
    /// issued if it arrives within [`ISSUE_WAIT`]. `None` when the host has no
    /// valid certificate.
    pub async fn get(&self, host: &str) -> Option<Arc<ServerConfig>> {
        let mut slot = self.slots.get(host)?.subscribe();
        let wait = slot.wait_for(|slot| slot.issued.as_ref().is_some_and(|issued| issued.valid()) || !slot.issuing);
        let slot = tokio::time::timeout(ISSUE_WAIT, wait).await.ok()?.ok()?;
        slot.issued.as_ref().filter(|issued| issued.valid()).map(|issued| issued.tls.clone())
    }

    /// Issue every host's certificate, and renew each, until `shutdown`.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut tasks = JoinSet::new();
        for host in self.slots.keys().cloned() {
            let store = Arc::clone(&self);
            let shutdown = shutdown.clone();
            tasks.spawn(async move { shutdown.run_until_cancelled(store.maintain(host)).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn maintain(&self, host: String) {
        let slot = &self.slots[&host];
        let mut retry = RETRY_MIN;
        loop {
            slot.send_modify(|slot| slot.issuing = true);
            let result = issue(self.signer.as_ref(), &host).await;
            let outcome = if result.is_ok() { "issued" } else { "failed" };
            self.issued.add(1, &[KeyValue::new("host", host.clone()), KeyValue::new("outcome", outcome)]);
            let wait = match result {
                Ok(issued) => {
                    tracing::info!(host, valid_until = %issued.valid_until, "forward proxy certificate issued");
                    retry = RETRY_MIN;
                    let wait = (issued.renew_at() - Utc::now()).to_std().unwrap_or_default().max(RETRY_MIN);
                    slot.send_replace(Slot { issued: Some(Arc::new(issued)), issuing: false });
                    wait
                }
                Err(error) => {
                    tracing::warn!(
                        host,
                        error = format!("{error:#}"),
                        retry_secs = retry.as_secs(),
                        "forward proxy certificate issuance failed"
                    );
                    slot.send_modify(|slot| slot.issuing = false);
                    let wait = retry;
                    retry = (retry * 2).min(RETRY_MAX);
                    wait
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// Generate a key pair, have the PKI sign a CSR for `host`, and check what it
/// returned.
async fn issue(signer: &dyn Signer, host: &str) -> Result<Issued> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating a key pair")?;
    let mut params = CertificateParams::new(vec![host.to_owned()]).context("building the CSR")?;
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, host);
    let csr = params.serialize_request(&key).context("signing the CSR")?.pem().context("encoding the CSR")?;
    let signed = signer.sign(&csr, host).await?;
    let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
    certified(host, &signed, &spki, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))
}

/// Check a signed certificate for `host` and build the TLS config that serves it.
pub(super) fn certified(host: &str, signed: &Signed, spki: &[u8], key: PrivateKeyDer<'static>) -> Result<Issued> {
    let leaf = pem_certificate(&signed.certificate).context("reading the signed certificate")?;
    let mut chain = vec![leaf];
    for pem in &signed.ca_chain {
        let der = pem_certificate(pem).context("reading the CA chain")?;
        if !chain.contains(&der) {
            chain.push(der);
        }
    }
    let parsed = chain
        .iter()
        .map(|der| X509Certificate::from_der(der).map(|(_, cert)| cert).map_err(|e| anyhow!("parsing a certificate: {e}")))
        .collect::<Result<Vec<_>>>()?;

    let leaf = &parsed[0];
    let names = leaf.subject_alternative_name().map_err(|e| anyhow!("reading the subject alternative names: {e}"))?;
    let names = names.map(|ext| ext.value.general_names.as_slice()).unwrap_or_default();
    let dns: Vec<&str> = names.iter().filter_map(|n| if let GeneralName::DNSName(d) = n { Some(*d) } else { None }).collect();
    ensure!(dns == [host], "the certificate names {dns:?}, not only {host}");
    ensure!(
        !names.iter().any(|n| matches!(n, GeneralName::IPAddress(_) | GeneralName::URI(_))),
        "the certificate has an IP or URI subject alternative name"
    );
    ensure!(leaf.validity().is_valid(), "the certificate is not valid now");
    ensure!(leaf.public_key().raw == spki, "the certificate is not for the generated key");
    for cert in &parsed {
        ensure!(permits(cert, host)?, "a name constraint in the chain does not permit {host}");
    }

    let served: Vec<usize> = (0..parsed.len()).filter(|&i| i == 0 || parsed[i].subject().as_raw() != parsed[i].issuer().as_raw()).collect();
    let valid_until = served.iter().map(|&i| parsed[i].validity().not_after.timestamp()).min().unwrap_or_default();
    let valid_until = DateTime::from_timestamp(valid_until, 0).context("reading notAfter")?;
    let not_before = DateTime::from_timestamp(leaf.validity().not_before.timestamp(), 0).context("reading notBefore")?;
    ensure!(Utc::now() < valid_until, "a certificate in the chain has expired");
    let served = served.into_iter().map(|i| chain[i].clone()).collect();

    let provider = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider();
    let key = CertifiedKey::from_der(served, key, &provider).context("loading the certificate and its key")?;
    let mut tls =
        ServerConfig::builder().with_no_client_auth().with_cert_resolver(Arc::new(SniCheck { host: host.to_owned(), key: Arc::new(key) }));
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Issued { tls: Arc::new(tls), not_before, valid_until })
}

fn pem_certificate(pem: &str) -> Result<CertificateDer<'static>> {
    CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow!("{e}"))
}

/// Whether `cert`'s DNS name constraints, if any, permit `host` (RFC 5280,
/// section 4.2.1.10).
fn permits(cert: &X509Certificate<'_>, host: &str) -> Result<bool> {
    let Some(constraints) = cert.name_constraints().map_err(|e| anyhow!("reading name constraints: {e}"))? else {
        return Ok(true);
    };
    let dns = |subtrees: &Option<Vec<x509_parser::extensions::GeneralSubtree<'_>>>| -> Vec<String> {
        subtrees
            .iter()
            .flatten()
            .filter_map(|tree| if let GeneralName::DNSName(d) = tree.base { Some(d.to_ascii_lowercase()) } else { None })
            .collect()
    };
    let within = |base: &str| match base.strip_prefix('.') {
        Some(below) => host.ends_with(&format!(".{below}")),
        None => base.is_empty() || host == base || host.ends_with(&format!(".{base}")),
    };
    let permitted = dns(&constraints.value.permitted_subtrees);
    let excluded = dns(&constraints.value.excluded_subtrees);
    Ok((permitted.is_empty() || permitted.iter().any(|b| within(b))) && !excluded.iter().any(|b| within(b)))
}

/// Serve the certificate only to a client whose SNI is the host.
struct SniCheck {
    host: String,
    key: Arc<CertifiedKey>,
}

impl fmt::Debug for SniCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SniCheck").field("host", &self.host).finish_non_exhaustive()
    }
}

impl ResolvesServerCert for SniCheck {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        let sni = sni.strip_suffix('.').unwrap_or(sni);
        sni.eq_ignore_ascii_case(&self.host).then(|| self.key.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use rcgen::SanType;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;
    use crate::forward_proxy::testpki::{self, TestPki};

    const HOST: &str = "capture.example";

    async fn handshake(tls: Arc<ServerConfig>, root: &CertificateDer<'static>, sni: &str) -> std::io::Result<()> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut stream = TlsAcceptor::from(tls).accept(server).await?;
            stream.write_all(b"ok").await?;
            stream.shutdown().await
        });
        let mut roots = RootCertStore::empty();
        roots.add(root.clone()).unwrap();
        let config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let name = ServerName::try_from(sni.to_owned()).unwrap();
        let result = async {
            let mut stream = TlsConnector::from(Arc::new(config)).connect(name, client).await?;
            let mut reply = Vec::new();
            stream.read_to_end(&mut reply).await?;
            assert_eq!(reply, b"ok");
            Ok(())
        }
        .await;
        let _ = server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn issues_a_certificate_through_the_pki_and_serves_only_its_host() {
        let pki = TestPki::start(&[HOST], &[HOST]).await;
        let store = Store::new(&[HOST.to_owned()], pki.signer());
        assert!(store.is_capture_host(HOST));
        assert!(!store.is_capture_host("other.example"));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(store.clone().run(shutdown.clone()));
        let tls = store.get(HOST).await.expect("a certificate within the wait");
        assert!(store.get("other.example").await.is_none());

        handshake(tls.clone(), &pki.root, HOST).await.unwrap();
        handshake(tls.clone(), &pki.root, "CAPTURE.example").await.unwrap();
        // A client asking for another name gets no certificate.
        assert!(handshake(tls, &pki.root, "other.example").await.is_err());
        assert_eq!(*pki.requests.lock().unwrap(), [HOST]);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_name_outside_the_role_gets_no_certificate() {
        let pki = TestPki::start(&[HOST], &[HOST]).await;
        let error = issue(pki.signer().as_ref(), "other.example").await.err().unwrap();
        assert!(format!("{error:#}").contains("common name other.example not allowed by this role"), "{error:#}");

        let store = Store::new(&["other.example".to_owned()], pki.signer());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(store.clone().run(shutdown.clone()));
        let started = tokio::time::Instant::now();
        assert!(store.get("other.example").await.is_none());
        assert!(started.elapsed() < ISSUE_WAIT, "a failed issuance ends the wait");
        assert_eq!(*pki.requests.lock().unwrap(), ["other.example", "other.example"]);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_certificate_that_is_not_exactly_for_the_host() {
        let cases: Vec<(&str, testpki::Tamper)> = vec![
            ("not only capture.example", Box::new(testpki::add_san(SanType::DnsName("other.example".try_into().unwrap())))),
            ("IP or URI", Box::new(testpki::add_san(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))))),
            ("IP or URI", Box::new(testpki::add_san(SanType::URI("https://capture.example/".try_into().unwrap())))),
            ("not for the generated key", Box::new(|csr| csr.public_key = testpki::other_public_key())),
            (
                "not valid now",
                Box::new(|csr| {
                    csr.params.not_before = rcgen::date_time_ymd(2000, 1, 1);
                    csr.params.not_after = rcgen::date_time_ymd(2001, 1, 1);
                }),
            ),
        ];
        for (expected, tamper) in cases {
            let pki = TestPki::start(&[HOST], &[HOST]).await;
            pki.tamper(tamper);
            let error = issue(pki.signer().as_ref(), HOST).await.err().unwrap();
            assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
        }

        // An intermediate whose name constraint leaves the host out would fail at every client.
        let pki = TestPki::start(&[HOST], &["elsewhere.example"]).await;
        let error = issue(pki.signer().as_ref(), HOST).await.err().unwrap();
        assert!(format!("{error:#}").contains("name constraint"), "{error:#}");
    }

    #[test]
    fn dns_name_constraints_follow_rfc_5280() {
        let check = |permitted: &[&str], excluded: &[&str], host: &str| {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params.name_constraints = Some(rcgen::NameConstraints {
                permitted_subtrees: permitted.iter().map(|n| rcgen::GeneralSubtree::DnsName((*n).into())).collect(),
                excluded_subtrees: excluded.iter().map(|n| rcgen::GeneralSubtree::DnsName((*n).into())).collect(),
            });
            let cert = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
            permits(&X509Certificate::from_der(cert.der()).unwrap().1, host).unwrap()
        };
        assert!(check(&["capture.example"], &[], "capture.example"));
        assert!(check(&["example"], &[], "capture.example"));
        assert!(!check(&["other.example"], &[], "capture.example"));
        assert!(!check(&["xcapture.example"], &[], "capture.example"));
        assert!(!check(&[".capture.example"], &[], "capture.example"));
        assert!(!check(&["capture.example"], &["capture.example"], "capture.example"));
        assert!(check(&[], &["other.example"], "capture.example"));
    }
}
