//! AI gateway: API-key upstream providers (Vercel AI Gateway, Mistral, any
//! OpenAI-compatible server such as llama.cpp), plus a raw passthrough
//! (`/<provider>/...`) for provider-specific APIs such as Mistral OCR / batch /
//! files and Vercel's credits / generation / report endpoints. Model naming,
//! aliases and fallbacks are handled by client policies.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::accounts::AccountKind;
use crate::config::{GatewayConfig, ProviderConfig, ProviderKind};
use crate::context::RequestSession;
use crate::db::Db;
use crate::export::Capture;
use crate::limits::Limiter;
use crate::policy::Target;
use crate::proxy::{self, Accounting, set_header};
use crate::secrets::Secrets;

const KEY_TTL: Duration = Duration::from_secs(300);

/// Where provider accounts added at runtime are stored, one secret per
/// account: the provider fields and `api_key`. The config file's providers
/// keep only their key here (`{ api_key }`), by default.
pub const PROVIDERS_PREFIX: &str = "providers";

/// Where a provider account is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderSource {
    /// `[gateway.providers.<name>]` in the config file; read-only at runtime.
    Config,
    /// Added through the admin API; stored in the vault.
    Vault,
}

#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub name: String,
    pub cfg: ProviderConfig,
    pub source: ProviderSource,
}

impl ProviderEntry {
    /// The vault secret that holds the provider's `api_key`.
    pub fn secret(&self) -> String {
        self.cfg.secret.clone().unwrap_or_else(|| format!("{PROVIDERS_PREFIX}/{}", self.name))
    }
}

/// Why adding, changing or removing a provider account failed.
#[derive(Debug, thiserror::Error)]
pub enum ProviderChangeError {
    #[error("no provider account named {0}")]
    NotFound(String),
    /// The name is taken, comes from the config file, or changed concurrently.
    #[error("{0}")]
    Conflict(String),
    #[error("{0:#}")]
    Invalid(anyhow::Error),
    #[error("{0:#}")]
    Store(anyhow::Error),
}

struct CachedKey {
    /// `None` for an account that needs no key.
    key: Option<String>,
    at: Instant,
    cfg: ProviderConfig,
}

pub struct Gateway {
    /// The config file's providers, disabled ones included.
    config: BTreeMap<String, ProviderConfig>,
    /// Config-file and vault providers, disabled ones included.
    providers: RwLock<BTreeMap<String, Arc<ProviderEntry>>>,
    /// Serializes changes to the vault providers (reload, put, remove).
    changes: tokio::sync::Mutex<()>,
    secrets: Secrets,
    http: reqwest::Client,
    db: Db,
    /// Cached keys, each with the provider config it was read with.
    keys: RwLock<HashMap<String, CachedKey>>,
    /// Bumped whenever cached keys are dropped, so a vault read that started
    /// before a key change can't put the old key back in the cache.
    key_generation: std::sync::atomic::AtomicU64,
    limiter: Arc<Limiter>,
    catalog: Option<Arc<crate::catalog::ModelCatalog>>,
}

impl Gateway {
    /// A gateway with the config file's providers. [`Gateway::reload`] adds the vault's.
    pub fn new(cfg: &GatewayConfig, secrets: Secrets, http: reqwest::Client, db: Db) -> Self {
        let providers = cfg
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), Arc::new(ProviderEntry { name: k.clone(), cfg: v.clone(), source: ProviderSource::Config })))
            .collect();
        Self {
            config: cfg.providers.clone(),
            providers: RwLock::new(providers),
            changes: Default::default(),
            secrets,
            http,
            db,
            keys: RwLock::new(HashMap::new()),
            key_generation: Default::default(),
            catalog: None,
            limiter: Default::default(),
        }
    }

    /// Snapshot model prices with each request, and keep the catalog's
    /// provider list in step with the gateway's.
    pub fn with_catalog(mut self, catalog: Arc<crate::catalog::ModelCatalog>) -> Self {
        catalog.set_gateway_kinds(self.kinds());
        self.catalog = Some(catalog);
        self
    }

    /// Count response tokens against client rate limits.
    pub fn with_limiter(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = limiter;
        self
    }

    /// An enabled provider.
    fn get(&self, name: &str) -> Option<Arc<ProviderEntry>> {
        self.providers.read().get(name).filter(|p| p.cfg.is_enabled()).cloned()
    }

    /// Every provider account, disabled ones included.
    pub fn entries(&self) -> Vec<Arc<ProviderEntry>> {
        self.providers.read().values().cloned().collect()
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn kind(&self, name: &str) -> Option<ProviderKind> {
        self.get(name).map(|p| p.cfg.kind)
    }

    /// The enabled providers and their kinds.
    pub fn kinds(&self) -> BTreeMap<String, ProviderKind> {
        self.providers.read().iter().filter(|(_, p)| p.cfg.is_enabled()).map(|(k, v)| (k.clone(), v.cfg.kind)).collect()
    }

    // ---- provider accounts stored in the vault

    /// Read the provider accounts stored in the vault and apply them. A secret
    /// without `kind` is a config-file provider's key, not an account. Invalid
    /// accounts, and names the config file already uses, are skipped.
    pub async fn reload(&self) -> Result<usize> {
        let _changes = self.changes.lock().await;
        self.reload_locked().await
    }

    async fn reload_locked(&self) -> Result<usize> {
        let mut next: BTreeMap<String, Arc<ProviderEntry>> =
            self.providers.read().iter().filter(|(_, p)| p.source == ProviderSource::Config).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut n = 0;
        for name in self.secrets.list(PROVIDERS_PREFIX).await? {
            if name.ends_with('/') {
                continue;
            }
            let Some(data) = self.secrets.get(&format!("{PROVIDERS_PREFIX}/{name}")).await? else { continue };
            if !data.contains_key("kind") {
                continue;
            }
            if self.config.contains_key(&name) {
                tracing::warn!(provider = %name, "provider account in the vault ignored: the config file defines one with that name");
                continue;
            }
            match stored_config(data).and_then(|cfg| cfg.validate_account(&name).map(|()| cfg)) {
                Ok(cfg) => {
                    n += 1;
                    next.insert(name.clone(), Arc::new(ProviderEntry { name, cfg, source: ProviderSource::Vault }));
                }
                Err(e) => tracing::warn!(provider = %name, error = %format!("{e:#}"), "invalid provider account in the vault ignored"),
            }
        }
        // A removed or changed provider must not keep a key cached for its old endpoint.
        let stale: Vec<String> = {
            let current = self.providers.read();
            current.iter().filter(|(k, e)| next.get(*k).is_none_or(|n| n.cfg != e.cfg)).map(|(k, _)| k.clone()).collect()
        };
        *self.providers.write() = next;
        for name in stale {
            self.forget_key(&name);
        }
        if let Some(c) = &self.catalog {
            c.set_gateway_kinds(self.kinds());
        }
        Ok(n)
    }

    /// Add (`create`) or change a provider account stored in the vault. A
    /// change without `api_key` keeps the stored key. Adding a name whose key
    /// is already stored (a provider moved out of the config file) keeps that
    /// key too.
    pub async fn put_provider(
        &self,
        name: &str,
        cfg: ProviderConfig,
        api_key: Option<String>,
        create: bool,
    ) -> Result<(), ProviderChangeError> {
        if self.config.contains_key(name) {
            return Err(ProviderChangeError::Conflict(format!("provider {name} is defined in the config file")));
        }
        cfg.validate_account(name).map_err(ProviderChangeError::Invalid)?;
        let api_key = api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        let _changes = self.changes.lock().await;
        let path = format!("{PROVIDERS_PREFIX}/{name}");
        let (current, version) = self.secrets.get_versioned(&path).await.map_err(ProviderChangeError::Store)?;
        let exists = current.as_ref().is_some_and(|d| d.contains_key("kind"));
        if create && exists {
            return Err(ProviderChangeError::Conflict(format!("a provider account named {name} already exists")));
        }
        if !create && !exists {
            return Err(ProviderChangeError::NotFound(name.to_string()));
        }
        let kept = current.as_ref().and_then(|d| d.get("api_key")).and_then(Value::as_str).filter(|k| !k.is_empty()).map(str::to_string);
        let key = api_key.or(kept);
        if !cfg.no_auth && key.is_none() {
            return Err(ProviderChangeError::Invalid(anyhow::anyhow!("provider {name}: give an API key, or set no_auth")));
        }
        let mut data = match serde_json::to_value(&cfg).map_err(|e| ProviderChangeError::Store(e.into()))? {
            Value::Object(m) => m,
            _ => unreachable!("a provider config is an object"),
        };
        data.retain(|_, v| !v.is_null());
        if let Some(k) = key {
            data.insert("api_key".into(), Value::String(k));
        }
        self.secrets.put_cas(&path, data, version).await.map_err(cas_error)?;
        self.forget_key(name);
        self.reload_locked().await.map_err(ProviderChangeError::Store)?;
        Ok(())
    }

    /// Remove a provider account stored in the vault, with its key.
    pub async fn remove_provider(&self, name: &str) -> Result<(), ProviderChangeError> {
        if self.config.contains_key(name) {
            return Err(ProviderChangeError::Conflict(format!("provider {name} is defined in the config file; remove it there")));
        }
        let _changes = self.changes.lock().await;
        if self.providers.read().get(name).is_none_or(|p| p.source != ProviderSource::Vault) {
            return Err(ProviderChangeError::NotFound(name.to_string()));
        }
        // Empty the secret with check-and-set, so a change another instance
        // made meanwhile is not lost, then destroy the versions that held the
        // key. The empty secret is not an account; adding the name again reuses it.
        let path = format!("{PROVIDERS_PREFIX}/{name}");
        let (current, version) = self.secrets.get_versioned(&path).await.map_err(ProviderChangeError::Store)?;
        if !current.as_ref().is_some_and(|d| d.contains_key("kind")) {
            return Err(ProviderChangeError::NotFound(name.to_string()));
        }
        self.secrets.put_cas(&path, Map::new(), version).await.map_err(cas_error)?;
        let versions: Vec<u64> = (1..=version).collect();
        if let Err(e) = self.secrets.destroy_versions(&path, &versions).await {
            tracing::warn!(provider = %name, error = %format!("{e:#}"), "removed provider account; destroying its old key versions failed");
        }
        self.forget_key(name);
        self.reload_locked().await.map_err(ProviderChangeError::Store)?;
        Ok(())
    }

    /// Store a provider's API key, in config-file and vault providers alike.
    /// Other fields of the secret are kept.
    pub async fn set_key(&self, name: &str, api_key: &str) -> Result<(), ProviderChangeError> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(ProviderChangeError::Invalid(anyhow::anyhow!("the API key is empty")));
        }
        let _changes = self.changes.lock().await;
        // Looked up under the lock, so a removal can't slip in before the write.
        let entry = self.providers.read().get(name).cloned().ok_or_else(|| ProviderChangeError::NotFound(name.to_string()))?;
        let path = entry.secret();
        let (current, version) = self.secrets.get_versioned(&path).await.map_err(ProviderChangeError::Store)?;
        // An account removed by another instance stays removed.
        if entry.source == ProviderSource::Vault && !current.as_ref().is_some_and(|d| d.contains_key("kind")) {
            return Err(ProviderChangeError::NotFound(name.to_string()));
        }
        let mut data = current.unwrap_or_default();
        data.insert("api_key".into(), Value::String(api_key.to_string()));
        self.secrets.put_cas(&path, data, version).await.map_err(cas_error)?;
        self.forget_key(name);
        Ok(())
    }

    /// Drop a cached key, and stop reads already in flight from caching theirs.
    fn forget_key(&self, name: &str) {
        let mut keys = self.keys.write();
        self.key_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        keys.remove(name);
    }

    /// Whether the provider's key is stored.
    pub async fn has_key(&self, entry: &ProviderEntry) -> Result<bool> {
        if entry.cfg.no_auth {
            return Ok(false);
        }
        Ok(self
            .secrets
            .get(&entry.secret())
            .await?
            .is_some_and(|m| m.get("api_key").and_then(Value::as_str).is_some_and(|k| !k.is_empty())))
    }

    fn base_url(p: &ProviderConfig) -> String {
        let base = match (&p.base_url, p.kind) {
            (Some(b), _) => b.clone(),
            (None, ProviderKind::Vercel) => crate::accounts::VERCEL_BASE_URL.into(),
            (None, ProviderKind::Mistral) => crate::accounts::MISTRAL_BASE_URL.into(),
            (None, ProviderKind::OpenaiCompatible) => String::new(),
        };
        let base = base.trim_end_matches('/');
        base.strip_suffix("/v1").unwrap_or(base).to_string()
    }

    /// The config and key a request uses. They belong to one revision of the
    /// account: an account stored in the vault is read in one go, so a change
    /// made meanwhile can't send its new key to the old endpoint, or the old
    /// key to the new one.
    async fn credentials(&self, p: &ProviderEntry) -> Result<(ProviderConfig, Option<String>)> {
        if p.cfg.no_auth && p.source == ProviderSource::Config {
            return Ok((p.cfg.clone(), None));
        }
        let name = &p.name;
        if let Some(c) = self.keys.read().get(name)
            && c.at.elapsed() < KEY_TTL
            && c.cfg == p.cfg
        {
            return Ok((c.cfg.clone(), c.key.clone()));
        }
        let path = p.secret();
        let generation = self.key_generation.load(std::sync::atomic::Ordering::SeqCst);
        let data = self.secrets.get(&path).await?.unwrap_or_default();
        let cfg = match p.source {
            ProviderSource::Config => p.cfg.clone(),
            ProviderSource::Vault => {
                anyhow::ensure!(data.contains_key("kind"), "provider account {name} was removed");
                let cfg = stored_config(data.clone())?;
                cfg.validate_account(name)?;
                cfg
            }
        };
        let key = if cfg.no_auth {
            None
        } else {
            Some(
                data.get("api_key")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                    .map(str::to_string)
                    .with_context(|| format!("no api_key in vault secret {path} for provider {name}"))?,
            )
        };
        let mut keys = self.keys.write();
        if self.key_generation.load(std::sync::atomic::Ordering::SeqCst) == generation {
            keys.insert(name.to_string(), CachedKey { key: key.clone(), at: Instant::now(), cfg: cfg.clone() });
        }
        Ok((cfg, key))
    }

    pub fn invalidate_keys(&self) {
        let mut keys = self.keys.write();
        self.key_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        keys.clear();
    }

    /// Send with the provider's current config and key. Returns the kind of
    /// that config, which a change on another instance can have made newer
    /// than [`Self::kind`].
    async fn send(
        &self,
        provider: &str,
        method: Method,
        path_and_query: &str,
        mut headers: HeaderMap,
        body: reqwest::Body,
    ) -> Result<(reqwest::Response, ProviderKind)> {
        let p = self.get(provider).with_context(|| format!("unknown provider {provider}"))?;
        let (cfg, key) = self.credentials(&p).await?;
        let url = format!("{}{}", Self::base_url(&cfg), path_and_query);
        if let Some(key) = key {
            set_header(&mut headers, "authorization", &format!("Bearer {key}"));
            if path_and_query.starts_with("/v1/messages") {
                set_header(&mut headers, "x-api-key", &key);
            }
        }
        for (k, v) in &cfg.headers {
            if let (Ok(name), Ok(val)) = (k.parse::<axum::http::HeaderName>(), v.parse::<axum::http::HeaderValue>()) {
                headers.insert(name, val);
            }
        }
        let resp = self.http.request(method, &url).headers(headers).body(body).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // The key may have been rotated in the vault.
            self.forget_key(provider);
        }
        Ok((resp, cfg.kind))
    }

    fn window(&self, provider: &str, model: &str) -> Option<u64> {
        self.catalog.as_ref().and_then(|c| c.context_window(provider, model))
    }

    /// Send a JSON request (`/v1/chat/completions`, `/v1/embeddings`,
    /// `/v1/ocr`, `/v1/responses`, `/v1/messages`, ...) to one target.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        target: &Target,
        method: Method,
        path: &str,
        headers: HeaderMap,
        mut body: Value,
        client_id: Option<&str>,
        requested_model: &str,
        session: Option<&RequestSession>,
        capture: Option<&Arc<Capture>>,
    ) -> Response {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(target.model.clone()));
            if self.kind(&target.provider) == Some(ProviderKind::Mistral) {
                normalize_mistral(path, obj);
            }
        }
        let mut accounting = Accounting::new(self.db.clone(), &target.provider, path, client_id, Some(requested_model))
            .with_prices(self.catalog.as_ref().map(|c| c.metadata(&target.provider, &target.model).values))
            .with_limiter(&self.limiter)
            .with_session(session.map(|s| s.track(&target.model, self.window(&target.provider, &target.model))))
            .with_capture(capture.cloned())
            .with_target(self.kind(&target.provider).map(AccountKind::from), &target.model);
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        match self.send(&target.provider, method, path, headers, reqwest::Body::from(bytes)).await {
            Ok((r, kind)) => {
                accounting.set_account_kind(kind.into());
                proxy::relay(r, Some(accounting))
            }
            Err(e) => {
                tracing::warn!(provider = %target.provider, error = %e, "gateway upstream request failed");
                accounting.finish(502, Default::default());
                proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("{}: {e:#}", target.provider))
            }
        }
    }

    /// Raw passthrough to `<base>/<rest>` with the provider's credentials.
    #[allow(clippy::too_many_arguments)]
    pub async fn passthrough(
        &self,
        provider: &str,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: reqwest::Body,
        client_id: Option<&str>,
        model: Option<&str>,
        session: Option<&RequestSession>,
        capture: Option<&Arc<Capture>>,
    ) -> Response {
        let endpoint = path_and_query.split('?').next().unwrap_or(path_and_query);
        let prices = self.catalog.as_ref().and_then(|c| {
            let model = model?.strip_prefix(&format!("{provider}/")).unwrap_or(model?);
            Some(c.metadata(provider, model).values)
        });
        let upstream_model = model.map(|m| m.strip_prefix(&format!("{provider}/")).unwrap_or(m)).unwrap_or_default();
        let mut accounting = Accounting::new(self.db.clone(), provider, endpoint, client_id, model)
            .with_limiter(&self.limiter)
            .with_prices(prices)
            .with_session(session.map(|s| s.track(upstream_model, self.window(provider, upstream_model))))
            .with_capture(capture.cloned())
            .with_target(self.kind(provider).map(AccountKind::from), upstream_model);
        match self.send(provider, method, path_and_query, headers, body).await {
            Ok((r, kind)) => {
                accounting.set_account_kind(kind.into());
                proxy::relay(r, Some(accounting))
            }
            Err(e) => {
                accounting.finish(502, Default::default());
                proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("{provider}: {e:#}"))
            }
        }
    }

    /// The provider's model ids from `GET /v1/models`.
    pub async fn fetch_models(&self, name: &str) -> Result<Vec<String>> {
        let (resp, _) = self.send(name, Method::GET, "/v1/models", HeaderMap::new(), reqwest::Body::from(Vec::new())).await?;
        anyhow::ensure!(resp.status().is_success(), "model list returned {}", resp.status());
        let body: Value = resp.json().await?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
            .collect())
    }
}

fn cas_error(e: anyhow::Error) -> ProviderChangeError {
    match e.downcast::<crate::secrets::Conflict>() {
        Ok(c) => ProviderChangeError::Conflict(c.to_string()),
        Err(e) => ProviderChangeError::Store(e),
    }
}

/// A stored provider account's fields, without its key.
fn stored_config(mut data: Map<String, Value>) -> Result<ProviderConfig> {
    data.remove("api_key");
    serde_json::from_value(Value::Object(data)).context("reading the provider account")
}

/// Mistral rejects a few OpenAI parameter spellings with a 422.
fn normalize_mistral(path: &str, obj: &mut serde_json::Map<String, Value>) {
    if path != "/v1/chat/completions" {
        return;
    }
    if let Some(v) = obj.remove("max_completion_tokens") {
        obj.entry("max_tokens").or_insert(v);
    }
    if let Some(v) = obj.remove("seed") {
        obj.entry("random_seed").or_insert(v);
    }
    if let Some(tc) = obj.get("tool_choice")
        && (tc == "required" || tc.is_object())
    {
        obj.insert("tool_choice".into(), json!("any"));
    }
    obj.remove("stream_options");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryStore;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn gateway(name: &str, s: &MockServer) -> Gateway {
        let secrets: Secrets = Arc::new(MemoryStore::default());
        let mut cfg = GatewayConfig { providers: BTreeMap::new() };
        cfg.providers.insert(
            name.to_string(),
            ProviderConfig { kind: ProviderKind::OpenaiCompatible, base_url: Some(format!("{}/v1", s.uri())), ..Default::default() },
        );
        secrets.put(&format!("providers/{name}"), json!({"api_key": format!("key-{name}")}).as_object().unwrap().clone()).await.unwrap();
        Gateway::new(&cfg, secrets, reqwest::Client::new(), Db::in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn injects_key_and_target_model() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer key-b"))
            .and(body_partial_json(json!({"model": "m2"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2}})))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m2"}, {"id": "m3"}]})))
            .mount(&s)
            .await;
        let g = gateway("b", &s).await;
        let t = Target { provider: "b".into(), model: "m2".into() };
        let resp =
            g.execute(&t, Method::POST, "/v1/chat/completions", HeaderMap::new(), json!({"model": "x"}), None, "x", None, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(g.fetch_models("b").await.unwrap(), vec!["m2", "m3"]);
    }

    #[tokio::test]
    async fn set_key_writes_the_configured_secret() {
        let secrets: Secrets = Arc::new(MemoryStore::default());
        secrets.put("team/llm", json!({"api_key": "old", "note": "kept"}).as_object().unwrap().clone()).await.unwrap();
        let mut cfg = GatewayConfig { providers: BTreeMap::new() };
        cfg.providers.insert(
            "local".into(),
            ProviderConfig { base_url: Some("http://x".into()), secret: Some("team/llm".into()), ..Default::default() },
        );
        let g = Gateway::new(&cfg, secrets.clone(), reqwest::Client::new(), Db::in_memory().await.unwrap());
        g.set_key("local", " new ").await.unwrap();
        let stored = secrets.get("team/llm").await.unwrap().unwrap();
        assert_eq!((stored["api_key"].as_str(), stored["note"].as_str()), (Some("new"), Some("kept")));
        assert!(secrets.get("providers/local").await.unwrap().is_none());
        assert!(matches!(g.set_key("nope", "k").await, Err(ProviderChangeError::NotFound(_))));
    }

    #[tokio::test]
    async fn a_changed_account_drops_its_cached_key() {
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        for (server, key) in [(&a, "key-a"), (&b, "key-b")] {
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .and(header("authorization", format!("Bearer {key}").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m"}]})))
                .mount(server)
                .await;
        }
        let secrets: Secrets = Arc::new(MemoryStore::default());
        let g = Gateway::new(
            &GatewayConfig { providers: BTreeMap::new() },
            secrets.clone(),
            reqwest::Client::new(),
            Db::in_memory().await.unwrap(),
        );
        let cfg = |base: &str| ProviderConfig { base_url: Some(base.to_string()), ..Default::default() };
        g.put_provider("p", cfg(&a.uri()), Some("key-a".into()), true).await.unwrap();
        assert_eq!(g.fetch_models("p").await.unwrap(), vec!["m"]);

        // Another instance points the account at another server with another key.
        let moved = json!({"kind": "openai-compatible", "base_url": b.uri(), "api_key": "key-b"});
        secrets.put("providers/p", moved.as_object().unwrap().clone()).await.unwrap();
        g.reload().await.unwrap();
        assert_eq!(g.fetch_models("p").await.unwrap(), vec!["m"], "the new server gets the new key");

        // Removed by another instance: a new key does not bring it back.
        secrets.put("providers/p", Map::new()).await.unwrap();
        assert!(matches!(g.set_key("p", "k").await, Err(ProviderChangeError::NotFound(_))));
        assert_eq!(secrets.get("providers/p").await.unwrap(), Some(Map::new()));
    }

    #[tokio::test]
    async fn a_request_uses_one_revision_of_the_account() {
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer key-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m"}]})))
            .mount(&b)
            .await;
        let secrets: Secrets = Arc::new(MemoryStore::default());
        let g = Gateway::new(
            &GatewayConfig { providers: BTreeMap::new() },
            secrets.clone(),
            reqwest::Client::new(),
            Db::in_memory().await.unwrap(),
        );
        g.put_provider("p", ProviderConfig { base_url: Some(a.uri()), ..Default::default() }, Some("key-a".into()), true).await.unwrap();
        // A request that looked the account up before another instance moved it.
        let before = g.get("p").unwrap();
        let moved = json!({"kind": "openai-compatible", "base_url": b.uri(), "api_key": "key-b"});
        secrets.put("providers/p", moved.as_object().unwrap().clone()).await.unwrap();
        let (cfg, key) = g.credentials(&before).await.unwrap();
        assert_eq!((cfg.base_url.as_deref(), key.as_deref()), (Some(b.uri().as_str()), Some("key-b")));
        // Removed meanwhile: nothing is sent.
        secrets.put("providers/p", Map::new()).await.unwrap();
        assert!(g.credentials(&before).await.is_err());
        assert!(a.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_export_names_the_kind_the_request_was_sent_with() {
        let (upstream, clickhouse) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []}))).mount(&upstream).await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&clickhouse).await;
        let cfg = crate::config::ClickHouseConfig { url: clickhouse.uri(), create_table: false, flush_secs: 3600, ..Default::default() };
        let export = crate::export::Exporter::new(vec![crate::export::clickhouse::start(cfg, None, None).unwrap()]);
        let secrets: Secrets = Arc::new(MemoryStore::default());
        let g = Gateway::new(
            &GatewayConfig { providers: BTreeMap::new() },
            secrets.clone(),
            reqwest::Client::new(),
            Db::in_memory().await.unwrap(),
        );
        let cfg = ProviderConfig { base_url: Some(upstream.uri()), ..Default::default() };
        g.put_provider("p", cfg, Some("k".into()), true).await.unwrap();
        // Another instance changed the account's kind; this one has not synced yet.
        let changed = json!({"kind": "mistral", "base_url": upstream.uri(), "api_key": "k"});
        secrets.put("providers/p", changed.as_object().unwrap().clone()).await.unwrap();
        assert_eq!(g.kind("p"), Some(ProviderKind::OpenaiCompatible));
        let body = bytes::Bytes::from_static(b"{\"model\":\"m\",\"input\":\"a\"}");
        let capture = export.capture("/v1/embeddings", Some(&body));
        let resp = g
            .passthrough("p", Method::POST, "/v1/embeddings", HeaderMap::new(), body.into(), Some("c"), Some("p/m"), None, Some(&capture))
            .await;
        axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        export.shutdown().await;
        let inserted = clickhouse.received_requests().await.unwrap();
        let row: Value = serde_json::from_slice(&inserted[0].body).unwrap();
        assert_eq!(row["account_kind"], "mistral");
    }

    #[test]
    fn mistral_normalization() {
        let mut o = json!({"max_completion_tokens": 5, "seed": 1, "tool_choice": "required"}).as_object().unwrap().clone();
        normalize_mistral("/v1/chat/completions", &mut o);
        assert_eq!(o["max_tokens"], 5);
        assert_eq!(o["random_seed"], 1);
        assert_eq!(o["tool_choice"], "any");
    }
}
