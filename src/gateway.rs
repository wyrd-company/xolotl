//! AI gateway: API-key upstream providers (Vercel AI Gateway, Mistral, any
//! OpenAI-compatible server such as llama.cpp) addressed as
//! `<provider>/<model>`, with model aliases and fallbacks, plus a raw
//! passthrough (`/<provider>/...`) for provider-specific APIs such as Mistral
//! OCR / batch / files and Vercel's credits / generation / report endpoints.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use parking_lot::RwLock;
use serde_json::{Value, json};

use crate::config::{GatewayConfig, ProviderConfig, ProviderKind};
use crate::db::Db;
use crate::proxy::{self, Accounting, set_header};
use crate::secrets::Secrets;

const KEY_TTL: Duration = Duration::from_secs(300);
const MODELS_TTL: Duration = Duration::from_secs(600);

pub struct Gateway {
    providers: BTreeMap<String, ProviderConfig>,
    aliases: HashMap<String, Vec<String>>,
    secrets: Secrets,
    http: reqwest::Client,
    db: Db,
    keys: RwLock<HashMap<String, (Option<String>, Instant)>>,
    models: RwLock<Option<(Vec<Value>, Instant)>>,
}

/// A resolved upstream target.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub provider: String,
    pub model: String,
}

impl Gateway {
    pub fn new(cfg: &GatewayConfig, secrets: Secrets, http: reqwest::Client, db: Db) -> Self {
        let providers = cfg.providers.iter().filter(|(_, p)| p.is_enabled()).map(|(k, v)| (k.clone(), v.clone())).collect();
        let aliases = cfg.models.iter().map(|a| (a.name.clone(), a.targets.clone())).collect();
        Self { providers, aliases, secrets, http, db, keys: RwLock::new(HashMap::new()), models: RwLock::new(None) }
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    pub fn provider_names(&self) -> impl Iterator<Item = &String> {
        self.providers.keys()
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

    /// Resolve a client model name: an alias, or `<provider>/<model>` for a
    /// configured gateway provider (split on the first `/` only, so
    /// `vercel/anthropic/claude-x` targets model `anthropic/claude-x`).
    pub fn resolve(&self, model: &str) -> Option<Vec<Target>> {
        if let Some(targets) = self.aliases.get(model) {
            return Some(targets.iter().filter_map(|t| self.split(t)).collect());
        }
        self.split(model).map(|t| vec![t])
    }

    fn split(&self, s: &str) -> Option<Target> {
        let (p, m) = s.split_once('/')?;
        (self.has_provider(p) && !m.is_empty()).then(|| Target { provider: p.to_string(), model: m.to_string() })
    }

    pub fn is_alias(&self, model: &str) -> bool {
        self.aliases.contains_key(model)
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

    /// Unified JSON endpoint (`/v1/chat/completions`, `/v1/embeddings`,
    /// `/v1/ocr`, `/v1/responses`, `/v1/messages`, ...): try each target in
    /// order, falling back on rate limits, server errors and transport errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        targets: &[Target],
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Value,
        client_id: Option<&str>,
        requested_model: &str,
    ) -> Response {
        let mut last: Option<(Target, u16, reqwest::header::HeaderMap, Bytes)> = None;
        for (i, target) in targets.iter().enumerate() {
            let is_last = i + 1 == targets.len();
            let mut b = body.clone();
            if let Some(obj) = b.as_object_mut() {
                obj.insert("model".into(), Value::String(target.model.clone()));
                if self.providers.get(&target.provider).is_some_and(|p| p.kind == ProviderKind::Mistral) {
                    normalize_mistral(path, obj);
                }
            }
            let accounting = || {
                let mut a = Accounting::new(
                    self.db.clone(),
                    &target.provider,
                    path,
                    client_id,
                    Some(&format!("{}/{}", target.provider, target.model)),
                );
                a.record.model = Some(if requested_model.contains('/') {
                    requested_model.to_string()
                } else {
                    format!("{}/{}", target.provider, target.model)
                });
                a
            };
            let started = accounting();
            let resp = self
                .send(
                    &target.provider,
                    method.clone(),
                    path,
                    headers.clone(),
                    reqwest::Body::from(serde_json::to_vec(&b).unwrap_or_default()),
                )
                .await;
            match resp {
                Err(e) => {
                    tracing::warn!(provider = %target.provider, error = %e, "gateway upstream request failed");
                    if is_last && last.is_none() {
                        started.finish(502, Default::default());
                        return proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("{}: {e:#}", target.provider));
                    }
                }
                Ok(r) => {
                    let status = r.status().as_u16();
                    let retryable = status == 429 || (500..600).contains(&status);
                    if !retryable || is_last {
                        return proxy::relay(r, Some(started));
                    }
                    tracing::info!(provider = %target.provider, status, "falling back to next target");
                    let h = r.headers().clone();
                    let bytes = r.bytes().await.unwrap_or_default();
                    started.finish(status, Default::default());
                    last = Some((target.clone(), status, h, bytes));
                }
            }
        }
        match last {
            Some((_, status, h, bytes)) => proxy::relay_buffered(status, &h, bytes, None),
            None => proxy::error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "no usable target"),
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
        let accounting = Accounting::new(self.db.clone(), provider, endpoint, client_id, model);
        match self.send(provider, method, path_and_query, headers, body).await {
            Ok(r) => proxy::relay(r, Some(accounting)),
            Err(e) => {
                accounting.finish(502, Default::default());
                proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("{provider}: {e:#}"))
            }
        }
    }

    /// Aggregated model list (`<provider>/<id>` plus aliases), cached.
    pub async fn list_models(&self) -> Vec<Value> {
        if let Some((m, at)) = self.models.read().as_ref()
            && at.elapsed() < MODELS_TTL
        {
            return m.clone();
        }
        let mut out = Vec::new();
        for name in self.providers.keys() {
            let resp = self.send(name, Method::GET, "/v1/models", HeaderMap::new(), reqwest::Body::from(Vec::new())).await;
            let body = match resp {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.unwrap_or(Value::Null),
                Ok(r) => {
                    tracing::debug!(provider = %name, status = %r.status(), "model list unavailable");
                    continue;
                }
                Err(e) => {
                    tracing::debug!(provider = %name, error = %e, "model list unavailable");
                    continue;
                }
            };
            for m in body.get("data").and_then(Value::as_array).into_iter().flatten() {
                if let Some(id) = m.get("id").and_then(Value::as_str) {
                    out.push(json!({"id": format!("{name}/{id}"), "object": "model", "owned_by": name}));
                }
            }
        }
        for alias in self.aliases.keys() {
            out.push(json!({"id": alias, "object": "model", "owned_by": "xolotl"}));
        }
        *self.models.write() = Some((out.clone(), Instant::now()));
        out
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
    use crate::config::ModelAlias;
    use crate::secrets::MemoryStore;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn gateway(servers: &[(&str, &MockServer)], aliases: Vec<ModelAlias>) -> Gateway {
        let secrets: Secrets = Arc::new(MemoryStore::default());
        let mut cfg = GatewayConfig { providers: BTreeMap::new(), models: aliases };
        for (name, s) in servers {
            cfg.providers.insert(
                name.to_string(),
                ProviderConfig { kind: ProviderKind::OpenaiCompatible, base_url: Some(format!("{}/v1", s.uri())), ..Default::default() },
            );
            secrets
                .put(&format!("providers/{name}"), json!({"api_key": format!("key-{name}")}).as_object().unwrap().clone())
                .await
                .unwrap();
        }
        Gateway::new(&cfg, secrets, reqwest::Client::new(), Db::in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn resolves_nested_models_and_aliases() {
        let s = MockServer::start().await;
        let g = gateway(
            &[("vercel", &s)],
            vec![ModelAlias { name: "fast".into(), targets: vec!["vercel/openai/gpt-5-mini".into(), "nope/x".into()] }],
        )
        .await;
        assert_eq!(g.resolve("vercel/anthropic/claude-x").unwrap()[0].model, "anthropic/claude-x");
        assert!(g.resolve("claude-sonnet").is_none());
        assert!(g.resolve("unknown/x").is_none());
        let fast = g.resolve("fast").unwrap();
        assert_eq!(fast.len(), 1, "unknown providers in aliases are dropped");
        assert_eq!(fast[0].provider, "vercel");
    }

    #[tokio::test]
    async fn falls_back_on_429_and_injects_key() {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(ResponseTemplate::new(429)).mount(&a).await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer key-b"))
            .and(body_partial_json(json!({"model": "m2"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2}})))
            .mount(&b)
            .await;
        let g = gateway(&[("a", &a), ("b", &b)], vec![ModelAlias { name: "x".into(), targets: vec!["a/m1".into(), "b/m2".into()] }]).await;
        let targets = g.resolve("x").unwrap();
        let resp = g.execute(&targets, Method::POST, "/v1/chat/completions", HeaderMap::new(), json!({"model": "x"}), None, "x").await;
        assert_eq!(resp.status(), 200);
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
