//! OAuth logins to upstream MCP servers (`auth = "oauth"`), following the
//! MCP authorization spec through rmcp: protected resource metadata
//! discovery, client registration (pre-registered, Client ID Metadata
//! Document or Dynamic Client Registration), PKCE, RFC 8707 `resource`,
//! RFC 9207 `iss` checks and token refresh.
//!
//! One login per server instance. Its client registration and tokens live in
//! the vault at `mcp/<name>/oauth`; refreshes are serialized per server
//! because refresh tokens may be single-use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationCallback, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialRefreshGuard,
    CredentialStore, OAuthClientConfig, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, McpServerConfig};
use crate::db::now;
use crate::secrets::{Conflict, Secrets};

/// Redirect URI when neither `mcp.oauth.redirect_uri` nor `server.public_url`
/// is set. Nothing listens there: the user pastes the URL the browser ends on.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:33418/callback";
/// xolotl's own callback and client metadata document, under `server.public_url`.
pub const CALLBACK_PATH: &str = "/admin/mcp/oauth/callback";
pub const CLIENT_METADATA_PATH: &str = "/admin/mcp/oauth/client-metadata.json";
/// How long a started login can be completed.
const FLOW_TTL: Duration = Duration::from_secs(15 * 60);

fn public_url(cfg: &Config) -> Option<&str> {
    cfg.server.public_url.as_deref().map(|u| u.trim_end_matches('/'))
}

pub fn redirect_uri(cfg: &Config) -> String {
    if let Some(u) = &cfg.mcp.oauth.redirect_uri {
        return u.clone();
    }
    match public_url(cfg) {
        Some(base) => format!("{base}{CALLBACK_PATH}"),
        None => DEFAULT_REDIRECT_URI.into(),
    }
}

/// The client metadata document URL, when there is one to offer.
pub fn client_metadata_url(cfg: &Config) -> Option<String> {
    if let Some(u) = &cfg.mcp.oauth.client_metadata_url {
        return Some(u.clone());
    }
    public_url(cfg).filter(|b| b.starts_with("https://")).map(|b| format!("{b}{CLIENT_METADATA_PATH}"))
}

/// The Client ID Metadata Document xolotl serves (and that a self-hosted copy must match).
pub fn client_metadata(cfg: &Config) -> Option<Value> {
    let client_id = client_metadata_url(cfg)?;
    Some(json!({
        "client_id": client_id,
        "client_name": cfg.mcp.oauth.client_name,
        "redirect_uris": [redirect_uri(cfg)],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    }))
}

// ------------------------------------------------------------ storage

/// How xolotl got its client id at the authorization server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationKind {
    PreRegistered,
    ClientMetadataDocument,
    Dynamic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub kind: RegistrationKind,
    pub client_id: String,
    /// Issued by Dynamic Client Registration; pre-registered secrets stay in `mcp/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

/// The vault record at `mcp/<name>/oauth`.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registration: Option<Registration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credentials: Option<StoredCredentials>,
    /// Why the server needs a new login, once it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    needs_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signed_in_at: Option<i64>,
}

#[derive(Default)]
struct State {
    loaded: bool,
    record: Record,
    version: u64,
    /// The vault write of the current record failed; retried later.
    unsaved: bool,
}

/// What the admin API shows about an OAuth server's login.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OAuthStatus {
    pub signed_in: bool,
    pub signed_in_at: Option<i64>,
    /// When the access token expires, if the server said.
    pub expires_at: Option<i64>,
    pub refreshable: bool,
    pub scopes: Vec<String>,
    pub registration: Option<RegistrationKind>,
    pub issuer: Option<String>,
    pub needs_login: Option<String>,
    pub unsaved: bool,
}

/// One server's login: an rmcp [`CredentialStore`] backed by the vault, with
/// the record cached in memory so every request doesn't read the vault.
pub struct OAuthStore {
    secrets: Secrets,
    key: String,
    state: tokio::sync::Mutex<State>,
    refresh: Arc<tokio::sync::Mutex<()>>,
}

impl OAuthStore {
    pub fn new(secrets: Secrets, server: &str) -> Arc<Self> {
        Arc::new(Self { secrets, key: format!("mcp/{server}/oauth"), state: Default::default(), refresh: Default::default() })
    }

    async fn locked(&self) -> Result<tokio::sync::MutexGuard<'_, State>, AuthError> {
        let mut st = self.state.lock().await;
        if !st.loaded {
            let (data, version) =
                self.secrets.get_versioned(&self.key).await.map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))?;
            st.record = match data {
                Some(m) => serde_json::from_value(Value::Object(m)).map_err(|e| AuthError::CredentialStoreError(e.to_string()))?,
                None => Record::default(),
            };
            st.version = version;
            st.loaded = true;
        }
        Ok(st)
    }

    /// Write the record. A failed write keeps the record in memory, marked
    /// unsaved: a refreshed token may already have spent the stored refresh token.
    async fn persist(&self, st: &mut State) {
        let data = match serde_json::to_value(&st.record) {
            Ok(Value::Object(m)) => m,
            _ => unreachable!("record serializes to an object"),
        };
        let mut result = self.secrets.put_cas(&self.key, data.clone(), st.version).await;
        if let Err(e) = &result
            && e.downcast_ref::<Conflict>().is_some()
        {
            // Changed outside xolotl; the in-memory record is the one in use.
            if let Ok((_, v)) = self.secrets.get_versioned(&self.key).await {
                result = self.secrets.put_cas(&self.key, data, v).await;
            }
        }
        match result {
            Ok(v) => {
                st.version = v;
                st.unsaved = false;
            }
            Err(e) => {
                tracing::error!(secret = %self.key, error = %format!("{e:#}"), "saving MCP OAuth credentials failed; keeping them in memory");
                st.unsaved = true;
            }
        }
    }

    pub async fn status(&self) -> OAuthStatus {
        let Ok(st) = self.locked().await else { return OAuthStatus::default() };
        let r = &st.record;
        let token = r.credentials.as_ref().and_then(|c| serde_json::to_value(c).ok());
        let token = token.as_ref().and_then(|c| c.get("token_response")).filter(|t| !t.is_null());
        let received = r.credentials.as_ref().and_then(|c| c.token_received_at);
        let expires_at = token.and_then(|t| t.get("expires_in").and_then(Value::as_u64)).zip(received).map(|(secs, at)| (at + secs) as i64);
        OAuthStatus {
            signed_in: token.is_some() && r.needs_login.is_none(),
            signed_in_at: r.signed_in_at,
            expires_at,
            refreshable: token.is_some_and(|t| t.get("refresh_token").is_some_and(|v| !v.is_null())),
            scopes: r.credentials.as_ref().map(|c| c.granted_scopes.clone()).unwrap_or_default(),
            registration: r.registration.as_ref().map(|g| g.kind),
            issuer: r.registration.as_ref().and_then(|g| g.issuer.clone()),
            needs_login: match (&r.needs_login, token) {
                (Some(reason), _) => Some(reason.clone()),
                (None, None) => Some("not signed in".into()),
                (None, Some(_)) => None,
            },
            unsaved: st.unsaved,
        }
    }

    pub async fn needs_login(&self) -> Option<String> {
        self.status().await.needs_login
    }

    /// Keeps the first reason: a rejected refresh token explains the 401 that follows.
    pub async fn mark_needs_login(&self, reason: &str) {
        if let Ok(mut st) = self.locked().await
            && st.record.needs_login.is_none()
        {
            st.record.needs_login = Some(reason.to_string());
            self.persist(&mut st).await;
        }
    }

    async fn registration(&self) -> Option<Registration> {
        self.locked().await.ok()?.record.registration.clone()
    }

    async fn set_registration(&self, reg: Registration) -> Result<()> {
        let mut st = self.locked().await?;
        st.record.registration = Some(reg);
        self.persist(&mut st).await;
        Ok(())
    }

    /// Forget the tokens (sign out). The client registration is kept for the next login.
    pub async fn sign_out(&self) -> Result<()> {
        let _guard = self.refresh.lock().await;
        let mut st = self.locked().await?;
        st.record.credentials = None;
        st.record.needs_login = Some("signed out".into());
        st.record.signed_in_at = None;
        self.persist(&mut st).await;
        anyhow::ensure!(!st.unsaved, "signed out in memory, but the vault write failed");
        Ok(())
    }

    pub async fn is_unsaved(&self) -> bool {
        self.state.lock().await.unsaved
    }

    pub async fn flush(&self) {
        let mut st = self.state.lock().await;
        if st.unsaved {
            self.persist(&mut st).await;
        }
    }
}

/// The [`CredentialStore`] handed to rmcp.
#[derive(Clone)]
pub struct StoreHandle(pub Arc<OAuthStore>);

#[async_trait::async_trait]
impl CredentialStore for StoreHandle {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        Ok(self.0.locked().await?.record.credentials.clone())
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let mut st = self.0.locked().await?;
        let fresh_login = st.record.credentials.as_ref().map(|c| c.token_received_at) != Some(credentials.token_received_at)
            && st.record.needs_login.is_some();
        st.record.credentials = Some(credentials);
        st.record.needs_login = None;
        if fresh_login || st.record.signed_in_at.is_none() {
            st.record.signed_in_at = Some(now());
        }
        self.0.persist(&mut st).await;
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let mut st = self.0.locked().await?;
        st.record.credentials = None;
        st.record.needs_login = Some("the authorization server changed".into());
        self.0.persist(&mut st).await;
        Ok(())
    }

    async fn on_refresh_token_rejected(&self, _credentials: &StoredCredentials) -> Result<(), AuthError> {
        let mut st = self.0.locked().await?;
        st.record.needs_login = Some("the refresh token was rejected".into());
        self.0.persist(&mut st).await;
        Ok(())
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        Ok(Some(CredentialRefreshGuard::new(self.0.refresh.clone().lock_owned().await)))
    }
}

// ------------------------------------------------------------ connecting

/// What a connection error says about the login, if anything.
pub fn auth_failure(error: &str) -> Option<String> {
    if error.contains("Insufficient scope") {
        Some("the server asked for more scopes; sign in again".into())
    } else if error.contains("Auth required") || error.contains("authorization required") || error.contains("refresh token was rejected") {
        Some("the server rejected the token".into())
    } else {
        None
    }
}

/// An rmcp client that sends the server's access token and refreshes it.
/// Fails when the server needs a login.
pub async fn authorized_client(
    name: &str,
    server: &McpServerConfig,
    store: &Arc<OAuthStore>,
    secrets: &Secrets,
    http: reqwest::Client,
) -> Result<AuthClient<reqwest::Client>> {
    if let Some(reason) = store.needs_login().await {
        anyhow::bail!("sign-in needed ({reason}): run `xolotl mcp login {name}` or use the web UI");
    }
    let mut manager = AuthorizationManager::new(server.url.as_str()).await?;
    manager.set_credential_store(StoreHandle(store.clone()));
    if !manager.initialize_from_store().await.context("restoring the OAuth login")? {
        store.mark_needs_login("the authorization server changed").await;
        anyhow::bail!("sign-in needed (the authorization server changed): run `xolotl mcp login {name}`");
    }
    // `initialize_from_store` restores only the client id.
    if let Some(reg) = store.registration().await {
        let secret = match reg.kind {
            RegistrationKind::PreRegistered => client_secret(secrets, name).await?,
            _ => reg.client_secret.clone(),
        };
        if let Some(secret) = secret {
            manager.configure_client(OAuthClientConfig::new(reg.client_id, reg.redirect_uri).with_client_secret(secret))?;
        }
    }
    Ok(AuthClient::new(http, manager))
}

async fn client_secret(secrets: &Secrets, server: &str) -> Result<Option<String>> {
    let secret = secrets.get(&format!("mcp/{server}")).await?.unwrap_or_default();
    Ok(secret.get("client_secret").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
}

// ------------------------------------------------------------ logins

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FlowStatus {
    Pending,
    Complete,
    Failed { error: String },
}

struct Flow {
    server: String,
    state: String,
    started: Instant,
    /// Taken while the code is exchanged, so a login completes only once.
    session: Option<AuthorizationSession>,
    status: FlowStatus,
}

/// A started login, as the admin API returns it.
#[derive(Debug, Clone, Serialize)]
pub struct FlowStart {
    pub flow_id: String,
    pub server: String,
    /// Open this in a browser.
    pub url: String,
    pub redirect_uri: String,
    /// True when the redirect comes back to xolotl, which then completes the
    /// login itself; otherwise the URL the browser ends on is pasted back.
    pub callback: bool,
    pub registration: RegistrationKind,
}

#[derive(Default)]
pub struct Flows {
    flows: Mutex<HashMap<String, Flow>>,
}

impl Flows {
    fn insert(&self, id: String, flow: Flow) {
        let mut flows = self.flows.lock();
        flows.retain(|_, f| f.started.elapsed() < FLOW_TTL);
        flows.insert(id, flow);
    }

    pub fn status(&self, id: &str) -> Option<(String, FlowStatus)> {
        self.flows.lock().get(id).filter(|f| f.started.elapsed() < FLOW_TTL).map(|f| (f.server.clone(), f.status.clone()))
    }

    pub fn by_state(&self, state: &str) -> Option<String> {
        self.flows.lock().iter().find(|(_, f)| f.state == state && f.started.elapsed() < FLOW_TTL).map(|(id, _)| id.clone())
    }

    /// Exchange the code in a redirect URL. Returns the server name.
    pub async fn complete(&self, id: &str, redirect_url: &str) -> Result<String> {
        let callback = AuthorizationCallback::from_redirect_url(redirect_url.trim())
            .map_err(|e| anyhow::anyhow!("{e}; paste the whole URL the browser ended on"))?;
        let (server, session) = {
            let mut flows = self.flows.lock();
            let flow = flows.get_mut(id).filter(|f| f.started.elapsed() < FLOW_TTL).context("no such login, or it expired; start again")?;
            anyhow::ensure!(flow.state == callback.csrf_token, "this URL belongs to a different login");
            let session = flow.session.take().context("this login was already completed")?;
            (flow.server.clone(), session)
        };
        let result = session.handle_callback_with_issuer(&callback.code, &callback.csrf_token, callback.issuer.as_deref()).await;
        let mut flows = self.flows.lock();
        let status = match &result {
            Ok(_) => FlowStatus::Complete,
            Err(e) => FlowStatus::Failed { error: e.to_string() },
        };
        if let Some(f) = flows.get_mut(id) {
            f.status = status;
        }
        result.map(|_| server).map_err(|e| anyhow::anyhow!("completing the login failed: {e}"))
    }
}

/// Ask the server for its OAuth challenge: an unauthenticated request should
/// get a 401 whose `WWW-Authenticate` points at its resource metadata.
async fn challenge(http: &reqwest::Client, url: &str) -> Option<String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}});
    let resp = http
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "server/discover")
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    (resp.status() == reqwest::StatusCode::UNAUTHORIZED)
        .then(|| resp.headers().get("www-authenticate")?.to_str().ok().map(str::to_string))
        .flatten()
}

/// Start a login: discovery, client registration and the authorization URL.
pub async fn start(
    cfg: &Config,
    name: &str,
    server: &McpServerConfig,
    store: &Arc<OAuthStore>,
    secrets: &Secrets,
    http: &reqwest::Client,
    flows: &Flows,
) -> Result<FlowStart> {
    let redirect = redirect_uri(cfg);
    let mut manager = AuthorizationManager::new(server.url.as_str()).await?;
    manager.set_credential_store(StoreHandle(store.clone()));
    let challenge = challenge(http, &server.url).await;
    let resolution = manager.resolve_metadata_from_challenge(challenge.as_deref()).await.context("discovering the authorization server")?;
    let issuer = resolution.metadata.issuer.clone();
    let cimd_supported =
        resolution.metadata.additional_fields.get("client_id_metadata_document_supported").and_then(Value::as_bool).unwrap_or(false);
    let has_registration_endpoint = resolution.metadata.registration_endpoint.is_some();
    manager.set_metadata(resolution.metadata);

    let scopes: Vec<String> = if server.oauth.scopes.is_empty() { manager.select_scopes(None, &[]) } else { server.oauth.scopes.clone() };
    let mut request =
        AuthorizationRequest::new(redirect.clone()).with_scopes(scopes.clone()).with_client_name(cfg.mcp.oauth.client_name.clone());
    let metadata_url = client_metadata_url(cfg);
    let kind = if let Some(client_id) = &server.oauth.client_id {
        request = request.with_preregistered_client(client_id.clone());
        if let Some(secret) = client_secret(secrets, name).await? {
            request = request.with_client_secret(secret);
        }
        store
            .set_registration(Registration {
                kind: RegistrationKind::PreRegistered,
                client_id: client_id.clone(),
                client_secret: None,
                redirect_uri: redirect.clone(),
                issuer: issuer.clone(),
            })
            .await?;
        RegistrationKind::PreRegistered
    } else if let Some(url) = metadata_url.filter(|_| cimd_supported) {
        request = request.with_client_metadata_url(url.clone());
        store
            .set_registration(Registration {
                kind: RegistrationKind::ClientMetadataDocument,
                client_id: url,
                client_secret: None,
                redirect_uri: redirect.clone(),
                issuer: issuer.clone(),
            })
            .await?;
        RegistrationKind::ClientMetadataDocument
    } else {
        // Reuse an earlier dynamic registration for the same issuer and redirect URI.
        let reusable =
            store.registration().await.filter(|r| r.kind == RegistrationKind::Dynamic && r.redirect_uri == redirect && r.issuer == issuer);
        let reg = match reusable {
            Some(r) => r,
            None => {
                anyhow::ensure!(
                    has_registration_endpoint,
                    "the authorization server supports neither dynamic client registration nor (as configured) client metadata \
                     documents; register a client there and set oauth.client_id for this server"
                );
                let refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
                let client = manager
                    .register_client(&cfg.mcp.oauth.client_name, &redirect, &refs)
                    .await
                    .context("dynamic client registration failed")?;
                let reg = Registration {
                    kind: RegistrationKind::Dynamic,
                    client_id: client.client_id,
                    client_secret: client.client_secret,
                    redirect_uri: redirect.clone(),
                    issuer: issuer.clone(),
                };
                store.set_registration(reg.clone()).await?;
                reg
            }
        };
        request = request.with_preregistered_client(reg.client_id);
        if let Some(secret) = reg.client_secret {
            request = request.with_client_secret(secret);
        }
        RegistrationKind::Dynamic
    };

    let session = AuthorizationSession::new(manager, request).await.map_err(|(_, e)| anyhow::anyhow!("starting the login failed: {e}"))?;
    let url = session.get_authorization_url().to_string();
    let state = url::Url::parse(&url)?
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .context("the authorization URL has no state")?;
    let id = crate::keys::random_id(16);
    let callback = public_url(cfg).is_some_and(|base| redirect == format!("{base}{CALLBACK_PATH}"));
    flows.insert(
        id.clone(),
        Flow { server: name.to_string(), state, started: Instant::now(), session: Some(session), status: FlowStatus::Pending },
    );
    Ok(FlowStart { flow_id: id, server: name.to_string(), url, redirect_uri: redirect, callback, registration: kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_and_metadata_urls() {
        let mut cfg = Config::default();
        assert_eq!(redirect_uri(&cfg), DEFAULT_REDIRECT_URI);
        assert!(client_metadata_url(&cfg).is_none());
        cfg.server.public_url = Some("http://xolotl.lan:8317/".into());
        assert_eq!(redirect_uri(&cfg), "http://xolotl.lan:8317/admin/mcp/oauth/callback");
        assert!(client_metadata_url(&cfg).is_none(), "client metadata documents must be https");
        cfg.server.public_url = Some("https://xolotl.example".into());
        assert_eq!(client_metadata_url(&cfg).unwrap(), "https://xolotl.example/admin/mcp/oauth/client-metadata.json");
        let doc = client_metadata(&cfg).unwrap();
        assert_eq!(doc["redirect_uris"], json!(["https://xolotl.example/admin/mcp/oauth/callback"]));
        cfg.mcp.oauth.redirect_uri = Some("http://localhost:9/cb".into());
        assert_eq!(redirect_uri(&cfg), "http://localhost:9/cb");
    }

    #[test]
    fn auth_failures() {
        assert!(auth_failure("connecting failed: Transport send error: Auth required").is_some());
        assert!(auth_failure("Insufficient scope").unwrap().contains("scopes"));
        assert!(auth_failure("connection refused").is_none());
    }
}
