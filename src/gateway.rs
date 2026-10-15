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
use serde_json::{Value, json};

use crate::config::{GatewayConfig, ProviderConfig, ProviderKind};
use crate::db::Db;
use crate::limits::Limiter;
use crate::policy::Target;
use crate::proxy::{self, Accounting, set_header};
use crate::secrets::Secrets;

const KEY_TTL: Duration = Duration::from_secs(300);

pub struct Gateway {
    providers: BTreeMap<String, ProviderConfig>,
    secrets: Secrets,
    http: reqwest::Client,
    db: Db,
    keys: RwLock<HashMap<String, (Option<String>, Instant)>>,
    limiter: Arc<Limiter>,
}

impl Gateway {
    pub fn new(cfg: &GatewayConfig, secrets: Secrets, http: reqwest::Client, db: Db) -> Self {
        let providers = cfg.providers.iter().filter(|(_, p)| p.is_enabled()).map(|(k, v)| (k.clone(), v.clone())).collect();
        Self { providers, secrets, http, db, keys: RwLock::new(HashMap::new()), limiter: Default::default() }
    }

    /// Count response tokens against client rate limits.
    pub fn with_limiter(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = limiter;
        self
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    pub fn kind(&self, name: &str) -> Option<ProviderKind> {
        self.providers.get(name).map(|p| p.kind)
    }

    pub fn kinds(&self) -> BTreeMap<String, ProviderKind> {
        self.providers.iter().map(|(k, v)| (k.clone(), v.kind)).collect()
    }

    fn base_url(p: &ProviderConfig) -> String {
        let base = match (&p.base_url, p.kind) {
            (Some(b), _) => b.clone(),
            (None, ProviderKind::Vercel) => "https://ai-gateway.vercel.sh".into(),
            (None, ProviderKind::Mistral) => "https://api.mistral.ai".into(),
            (None, ProviderKind::OpenaiCompatible) => String::new(),
        };
        let base = base.trim_end_matches('/');
        base.strip_suffix("/v1").unwrap_or(base).to_string()
    }

    async fn api_key(&self, name: &str) -> Result<Option<String>> {
        let p = self.providers.get(name).with_context(|| format!("unknown provider {name}"))?;
        if p.no_auth {
            return Ok(None);
        }
        if let Some((k, at)) = self.keys.read().get(name)
            && at.elapsed() < KEY_TTL
        {
            return Ok(k.clone());
        }
        let path = p.secret.clone().unwrap_or_else(|| format!("providers/{name}"));
        let key = self
            .secrets
            .get(&path)
            .await?
            .and_then(|m| m.get("api_key").and_then(Value::as_str).map(str::to_string))
            .with_context(|| format!("no api_key in vault secret {path} for provider {name}"))?;
        self.keys.write().insert(name.to_string(), (Some(key.clone()), Instant::now()));
        Ok(Some(key))
    }

    pub fn invalidate_keys(&self) {
        self.keys.write().clear();
    }

    async fn send(
        &self,
        provider: &str,
        method: Method,
        path_and_query: &str,
        mut headers: HeaderMap,
        body: reqwest::Body,
    ) -> Result<reqwest::Response> {
        let p = self.providers.get(provider).with_context(|| format!("unknown provider {provider}"))?;
        let url = format!("{}{}", Self::base_url(p), path_and_query);
        if let Some(key) = self.api_key(provider).await? {
            set_header(&mut headers, "authorization", &format!("Bearer {key}"));
            if path_and_query.starts_with("/v1/messages") {
                set_header(&mut headers, "x-api-key", &key);
            }
        }
        for (k, v) in &p.headers {
            if let (Ok(name), Ok(val)) = (k.parse::<axum::http::HeaderName>(), v.parse::<axum::http::HeaderValue>()) {
                headers.insert(name, val);
            }
        }
        let resp = self.http.request(method, &url).headers(headers).body(body).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // The key may have been rotated in the vault.
            self.keys.write().remove(provider);
        }
        Ok(resp)
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
    ) -> Response {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(target.model.clone()));
            if self.providers.get(&target.provider).is_some_and(|p| p.kind == ProviderKind::Mistral) {
                normalize_mistral(path, obj);
            }
        }
        let accounting =
            Accounting::new(self.db.clone(), &target.provider, path, client_id, Some(requested_model)).with_limiter(&self.limiter);
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        match self.send(&target.provider, method, path, headers, reqwest::Body::from(bytes)).await {
            Ok(r) => proxy::relay(r, Some(accounting)),
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
    ) -> Response {
        let endpoint = path_and_query.split('?').next().unwrap_or(path_and_query);
        let accounting = Accounting::new(self.db.clone(), provider, endpoint, client_id, model).with_limiter(&self.limiter);
        match self.send(provider, method, path_and_query, headers, body).await {
            Ok(r) => proxy::relay(r, Some(accounting)),
            Err(e) => {
                accounting.finish(502, Default::default());
                proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("{provider}: {e:#}"))
            }
        }
    }

    /// The provider's model ids from `GET /v1/models`.
    pub async fn fetch_models(&self, name: &str) -> Result<Vec<String>> {
        let resp = self.send(name, Method::GET, "/v1/models", HeaderMap::new(), reqwest::Body::from(Vec::new())).await?;
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
        let resp = g.execute(&t, Method::POST, "/v1/chat/completions", HeaderMap::new(), json!({"model": "x"}), None, "x").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(g.fetch_models("b").await.unwrap(), vec!["m2", "m3"]);
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
