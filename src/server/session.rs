//! Web UI sign-in through OpenID Connect (authorization code flow with PKCE,
//! as a confidential client) and an in-memory session behind an HttpOnly
//! cookie. Only members of `ui.oidc.admin_group` are signed in.
//!
//! A session authorizes only the admin API, and only together with its CSRF
//! token in `x-csrf-token` on every request. Browsers never add that header
//! to a cross-site request, so the cookie alone grants nothing.
//!
//! Sessions and sign-ins in progress don't survive a restart and aren't
//! shared between instances.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType, CoreGenderClaim, CoreJsonWebKey,
    CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreRevocableToken, CoreRevocationErrorResponse,
    CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::{
    AdditionalClaims, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IdTokenFields, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, StandardErrorResponse,
    StandardTokenResponse,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AppState, json_error};
use crate::config::OidcConfig;
use crate::keys::{Principal, random_id, sha256_hex};
use crate::policy::Policy;

pub const COOKIE: &str = "xolotl_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
pub const CALLBACK_PATH: &str = "/admin/session/callback";
/// Holds the `state` of a sign-in in progress, so the callback only
/// completes in the browser that started it.
const PENDING_COOKIE: &str = "xolotl_oidc";

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
/// Discovery documents, key sets and token responses are a few KiB.
const MAX_PROVIDER_RESPONSE: usize = 1024 * 1024;
/// ID token signatures accepted: the asymmetric ones (Pocket ID can rotate
/// to RS256, ES256 or EdDSA keys). Never HMAC with the client secret.
const SIGNING_ALGS: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EdDsa,
];
/// Sign-ins in progress kept at once. Starting one is unauthenticated, so
/// the oldest is dropped rather than refusing new ones.
const MAX_PENDING: usize = 256;

/// The `groups` claim (Pocket ID and most others send it for the `groups` scope).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct Groups {
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}
impl AdditionalClaims for Groups {}

type TokenResponse = StandardTokenResponse<
    IdTokenFields<Groups, EmptyExtraTokenFields, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm>,
    CoreTokenType,
>;

/// `CoreClient` with the `groups` claim, as built from discovery.
type Client = openidconnect::Client<
    Groups,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

struct Session {
    csrf: String,
    user: String,
    expires: Instant,
}

/// A sign-in sent to the provider, waiting for its callback. The provider
/// metadata (with its signing keys) is kept so the callback doesn't
/// discover again.
struct Pending {
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    metadata: CoreProviderMetadata,
    expires: Instant,
}

/// Live sessions, keyed by the SHA-256 of the cookie value, and sign-ins in
/// progress, keyed by their `state`.
#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Session>>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Sessions {
    /// Start a session for `user`: (cookie value, CSRF token).
    fn create(&self, user: String) -> (String, String) {
        let id = random_id(43);
        let csrf = random_id(32);
        let now = Instant::now();
        let mut live = self.live.lock();
        live.retain(|_, s| s.expires > now);
        live.insert(sha256_hex(&id), Session { csrf: csrf.clone(), user, expires: now + SESSION_TTL });
        (id, csrf)
    }

    /// The CSRF token and user of the session named by the request's cookie.
    fn session(&self, headers: &HeaderMap) -> Option<(String, String)> {
        let id = cookie(headers, COOKIE)?;
        let live = self.live.lock();
        live.get(&sha256_hex(&id)).filter(|s| s.expires > Instant::now()).map(|s| (s.csrf.clone(), s.user.clone()))
    }

    /// The admin principal for a request with a live session cookie and its CSRF token.
    pub fn principal(&self, headers: &HeaderMap) -> Option<Principal> {
        let (expected, _) = self.session(headers)?;
        let presented = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok())?;
        crate::keys::ct_eq(expected.as_bytes(), presented.as_bytes())
            .then(|| Principal { client_id: None, policy: Arc::new(Policy::unrestricted()) })
    }

    fn remove(&self, headers: &HeaderMap) -> Option<String> {
        let id = cookie(headers, COOKIE)?;
        self.live.lock().remove(&sha256_hex(&id)).map(|s| s.user)
    }

    /// Remember a sign-in in progress, dropping the oldest when too many are.
    fn begin(&self, state: String, p: Pending) {
        let mut pending = self.pending.lock();
        let now = Instant::now();
        pending.retain(|_, p| p.expires > now);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending.iter().min_by_key(|(_, p)| p.expires).map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(state, p);
    }

    /// Take a sign-in in progress; each can complete once.
    fn take(&self, state: &str) -> Option<Pending> {
        self.pending.lock().remove(state).filter(|p| p.expires > Instant::now())
    }
}

/// A cookie's value from the request's `Cookie` headers.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

/// Cookies are `Secure` when the UI is reached over HTTPS, as the
/// configured redirect URL says.
fn secure(cfg: Option<&OidcConfig>) -> &'static str {
    if cfg.is_some_and(|c| c.redirect_url.starts_with("https:")) { "; Secure" } else { "" }
}

fn set_cookie(cfg: Option<&OidcConfig>, value: &str, max_age: u64) -> HeaderValue {
    let secure = secure(cfg);
    HeaderValue::from_str(&format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"))
        .expect("cookie value is ASCII")
}

/// The pending-sign-in cookie. `SameSite=Lax`, because the callback is a
/// navigation from the provider's site; scoped to the callback path.
fn set_pending_cookie(cfg: &OidcConfig, value: &str, max_age: u64) -> HeaderValue {
    let secure = secure(Some(cfg));
    HeaderValue::from_str(&format!("{PENDING_COOKIE}={value}; Path={CALLBACK_PATH}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie value is ASCII")
}

/// HTTP client for the provider. It never follows redirects, as OAuth 2 requires.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .expect("building the OIDC HTTP client")
    })
}

async fn send(req: openidconnect::HttpRequest) -> Result<openidconnect::HttpResponse, std::io::Error> {
    let (parts, body) = req.into_parts();
    let mut resp = http_client()
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .map_err(std::io::Error::other)?;
    let mut out = axum::http::Response::builder().status(resp.status());
    if let Some(h) = out.headers_mut() {
        *h = resp.headers().clone();
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(std::io::Error::other)? {
        if body.len() + chunk.len() > MAX_PROVIDER_RESPONSE {
            return Err(std::io::Error::other("the provider's response is too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(out.body(body).expect("status and headers come from a valid response"))
}

fn client(cfg: &OidcConfig, metadata: CoreProviderMetadata, secret: String) -> anyhow::Result<Client> {
    Ok(Client::from_provider_metadata(metadata, ClientId::new(cfg.client_id.clone()), Some(ClientSecret::new(secret)))
        .set_redirect_uri(RedirectUrl::new(cfg.redirect_url.clone())?))
}

async fn client_secret(s: &AppState) -> anyhow::Result<String> {
    let oidc = s.secrets.get("oidc").await?.unwrap_or_default();
    oidc.get("client_secret")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the vault secret `oidc` has no client_secret"))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/session", get(status).delete(logout))
        .route("/session/login", get(login))
        .route("/session/callback", get(callback))
}

/// Whether the caller is signed in (with the CSRF token to use and who
/// they are), and whether sign-in is configured at all.
async fn status(State(s): State<AppState>, h: HeaderMap) -> Response {
    match s.sessions.session(&h) {
        Some((csrf, user)) => Json(json!({"authenticated": true, "csrf_token": csrf, "user": user})).into_response(),
        None => Json(json!({"authenticated": false, "configured": s.cfg.ui.oidc.is_some()})).into_response(),
    }
}

/// Back to the UI. A failed sign-in names one of a fixed set of reasons;
/// the details go to the log.
fn to_ui(error: Option<&str>) -> Response {
    let to = match error {
        Some(e) => format!("/?signin={e}"),
        None => "/".into(),
    };
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// Send the browser to the provider.
async fn login(State(s): State<AppState>) -> Response {
    let Some(cfg) = &s.cfg.ui.oidc else {
        return json_error(StatusCode::NOT_FOUND, "web UI sign-in is not configured ([ui.oidc])");
    };
    let started = async {
        let issuer = IssuerUrl::new(cfg.issuer.clone())?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &send).await?;
        let client = client(cfg, metadata.clone(), client_secret(&s).await?)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = client
            .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
            .add_scope(openidconnect::Scope::new("profile".into()))
            .add_scope(openidconnect::Scope::new("groups".into()))
            .set_pkce_challenge(challenge)
            .url();
        anyhow::Ok((url, state, Pending { nonce, verifier, metadata, expires: Instant::now() + PENDING_TTL }))
    };
    let (url, state, pending) = match started.await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "UI sign-in could not start");
            return to_ui(Some("unavailable"));
        }
    };
    s.sessions.begin(state.secret().clone(), pending);
    let mut resp = (StatusCode::SEE_OTHER, [(header::LOCATION, url.to_string())]).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_pending_cookie(cfg, state.secret(), PENDING_TTL.as_secs()));
    resp
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// The provider sends the browser back here with a code.
async fn callback(State(s): State<AppState>, h: HeaderMap, Query(q): Query<CallbackParams>) -> Response {
    let Some(cfg) = &s.cfg.ui.oidc else {
        return json_error(StatusCode::NOT_FOUND, "web UI sign-in is not configured ([ui.oidc])");
    };
    let mut resp = finish(&s, cfg, &h, q).await;
    resp.headers_mut().append(header::SET_COOKIE, set_pending_cookie(cfg, "", 0));
    resp
}

async fn finish(s: &AppState, cfg: &OidcConfig, h: &HeaderMap, q: CallbackParams) -> Response {
    // Only the browser that started the sign-in may finish it.
    let (Some(state), Some(ours)) = (q.state, cookie(h, PENDING_COOKIE)) else {
        return to_ui(Some("expired"));
    };
    if !crate::keys::ct_eq(state.as_bytes(), ours.as_bytes()) {
        return to_ui(Some("expired"));
    }
    let Some(pending) = s.sessions.take(&state) else {
        return to_ui(Some("expired"));
    };
    if let Some(e) = q.error {
        tracing::warn!(error = %e, "UI sign-in refused by the provider");
        return to_ui(Some("denied"));
    }
    let Some(code) = q.code else {
        return to_ui(Some("failed"));
    };

    let claims = async {
        let client = client(cfg, pending.metadata, client_secret(s).await?)?;
        let token = client.exchange_code(AuthorizationCode::new(code))?.set_pkce_verifier(pending.verifier).request_async(&send).await?;
        let id_token = openidconnect::TokenResponse::id_token(&token).ok_or_else(|| anyhow::anyhow!("no ID token in the response"))?;
        let verifier = client.id_token_verifier().set_allowed_algs(SIGNING_ALGS);
        anyhow::Ok(id_token.claims(&verifier, &pending.nonce)?.clone())
    };
    let claims = match claims.await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "UI sign-in failed");
            return to_ui(Some("failed"));
        }
    };
    let user = claims.preferred_username().map(|u| u.to_string()).unwrap_or_else(|| claims.subject().to_string());
    if !claims.additional_claims().groups.contains(&cfg.admin_group) {
        tracing::warn!(%user, group = %cfg.admin_group, "UI sign-in by a user outside the admin group");
        return to_ui(Some("forbidden"));
    }
    tracing::info!(%user, "UI sign-in");
    let (id, _) = s.sessions.create(user);
    let mut resp = to_ui(None);
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie(Some(cfg), &id, SESSION_TTL.as_secs()));
    resp
}

async fn logout(State(s): State<AppState>, h: HeaderMap) -> Response {
    if s.sessions.principal(&h).is_none() {
        return json_error(StatusCode::UNAUTHORIZED, "not signed in");
    }
    if let Some(user) = s.sessions.remove(&h) {
        tracing::info!(%user, "UI sign-out");
    }
    let mut resp = Json(json!({"authenticated": false})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie(s.cfg.ui.oidc.as_ref(), "", 0));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1; xolotl_session=abc"));
        assert_eq!(cookie(&h, COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "b"), None);
    }

    #[test]
    fn session_needs_its_csrf_token() {
        let s = Sessions::default();
        let (id, csrf) = s.create("someone".into());
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(&format!("{COOKIE}={id}")).unwrap());
        assert!(s.principal(&h).is_none());
        h.insert(CSRF_HEADER, HeaderValue::from_static("nope"));
        assert!(s.principal(&h).is_none());
        h.insert(CSRF_HEADER, HeaderValue::from_str(&csrf).unwrap());
        assert!(s.principal(&h).is_some());
        assert_eq!(s.remove(&h).as_deref(), Some("someone"));
        assert!(s.principal(&h).is_none());
    }

    #[test]
    fn too_many_sign_ins_drop_the_oldest() {
        let metadata: CoreProviderMetadata = serde_json::from_value(json!({
            "issuer": "https://id.example.test", "authorization_endpoint": "https://id.example.test/authorize",
            "jwks_uri": "https://id.example.test/jwks", "response_types_supported": ["code"],
            "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["RS256"],
        }))
        .unwrap();
        let s = Sessions::default();
        for i in 0..=MAX_PENDING {
            let expires = Instant::now() + PENDING_TTL + Duration::from_secs(i as u64);
            let p =
                Pending { nonce: Nonce::new_random(), verifier: PkceCodeVerifier::new("v".into()), metadata: metadata.clone(), expires };
            s.begin(format!("state-{i}"), p);
        }
        assert!(s.take("state-0").is_none(), "the oldest was dropped");
        assert!(s.take("state-1").is_some());
        assert!(s.take(&format!("state-{MAX_PENDING}")).is_some());
        assert!(s.take("state-1").is_none(), "each completes once");

        let expired = Pending {
            nonce: Nonce::new_random(),
            verifier: PkceCodeVerifier::new("v".into()),
            metadata,
            expires: Instant::now() - Duration::from_secs(1),
        };
        s.begin("late".into(), expired);
        assert!(s.take("late").is_none(), "an expired sign-in can't complete");
    }
}
