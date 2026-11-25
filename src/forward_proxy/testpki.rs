//! A test PKI: an `rcgen` root and a name-constrained intermediate behind a
//! wiremock OpenBao that answers `POST /v1/<mount>/sign/<role>`.

use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer, DnType, GeneralSubtree, IsCa, KeyPair,
    NameConstraints, SanType,
};
use serde_json::{Value, json};
use tokio_rustls::rustls::pki_types::CertificateDer;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::config::VaultConfig;
use crate::vault::VaultClient;

pub const MOUNT: &str = "pki_xolotl_proxy";
pub const ROLE: &str = "xolotl-forward-proxy";

/// Changes the test PKI makes to a certificate before signing it.
pub type Tamper = Box<dyn Fn(&mut CertificateSigningRequestParams) + Send + Sync>;

pub struct TestPki {
    pub server: MockServer,
    pub root: CertificateDer<'static>,
    /// Every `common_name` the PKI was asked to sign.
    pub requests: Arc<Mutex<Vec<String>>>,
    pub tamper: Arc<Mutex<Option<Tamper>>>,
}

struct Sign {
    allowed: Vec<String>,
    root_pem: String,
    intermediate: CertifiedIssuer<'static, KeyPair>,
    requests: Arc<Mutex<Vec<String>>>,
    tamper: Arc<Mutex<Option<Tamper>>>,
}

impl Respond for Sign {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let name = body["common_name"].as_str().unwrap().to_owned();
        self.requests.lock().unwrap().push(name.clone());
        if !self.allowed.contains(&name) {
            return ResponseTemplate::new(400).set_body_json(json!({"errors": [format!("common name {name} not allowed by this role")]}));
        }
        let mut csr = CertificateSigningRequestParams::from_pem(body["csr"].as_str().unwrap()).unwrap();
        csr.params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        csr.params.not_after = time::OffsetDateTime::now_utc() + time::Duration::hours(24);
        if let Some(tamper) = self.tamper.lock().unwrap().as_ref() {
            tamper(&mut csr);
        }
        let leaf = csr.signed_by(&self.intermediate).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({"data": {
            "certificate": leaf.pem(),
            "issuing_ca": self.intermediate.pem(),
            "ca_chain": [self.intermediate.pem(), self.root_pem],
        }}))
    }
}

impl TestPki {
    /// A PKI whose role signs `allowed`, under an intermediate constrained to
    /// `permitted`.
    pub async fn start(allowed: &[&str], permitted: &[&str]) -> Self {
        let mut root = CertificateParams::new(Vec::<String>::new()).unwrap();
        root.distinguished_name.push(DnType::CommonName, "xolotl test root");
        root.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let root = CertifiedIssuer::self_signed(root, KeyPair::generate().unwrap()).unwrap();
        let mut intermediate = CertificateParams::new(Vec::<String>::new()).unwrap();
        intermediate.distinguished_name.push(DnType::CommonName, "xolotl forward proxy");
        intermediate.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        intermediate.name_constraints = Some(NameConstraints {
            permitted_subtrees: permitted.iter().map(|name| GeneralSubtree::DnsName((*name).to_owned())).collect(),
            excluded_subtrees: vec![],
        });
        let intermediate = CertifiedIssuer::signed_by(intermediate, KeyPair::generate().unwrap(), &root).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"auth": {"client_token": "tok", "lease_duration": 3600}})))
            .mount(&server)
            .await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tamper = Arc::new(Mutex::new(None));
        let sign = Sign {
            allowed: allowed.iter().map(|name| (*name).to_owned()).collect(),
            root_pem: root.pem(),
            intermediate,
            requests: requests.clone(),
            tamper: tamper.clone(),
        };
        Mock::given(method("POST")).and(path(format!("/v1/{MOUNT}/sign/{ROLE}"))).respond_with(sign).mount(&server).await;
        Self { server, root: root.der().clone(), requests, tamper }
    }

    pub fn vault(&self) -> VaultClient {
        let mut config = VaultConfig { address: self.server.uri(), ..Default::default() };
        config.approle.role_id = Some("role".into());
        config.approle.secret_id = Some("secret".into());
        VaultClient::new(&config).unwrap()
    }

    pub fn signer(&self) -> Arc<super::certs::PkiSigner> {
        Arc::new(super::certs::PkiSigner { vault: self.vault(), mount: MOUNT.into(), role: ROLE.into() })
    }

    pub fn tamper(&self, tamper: impl Fn(&mut CertificateSigningRequestParams) + Send + Sync + 'static) {
        *self.tamper.lock().unwrap() = Some(Box::new(tamper));
    }
}

/// The public key of a key pair other than the CSR's.
pub fn other_public_key() -> rcgen::PublicKey {
    let key = KeyPair::generate().unwrap();
    let csr = CertificateParams::new(vec!["other.example".to_owned()]).unwrap().serialize_request(&key).unwrap();
    CertificateSigningRequestParams::from_pem(&csr.pem().unwrap()).unwrap().public_key
}

/// One more subject alternative name than the CSR asked for.
pub fn add_san(san: SanType) -> impl Fn(&mut CertificateSigningRequestParams) + Send + Sync + 'static {
    move |csr| csr.params.subject_alt_names.push(san.clone())
}
