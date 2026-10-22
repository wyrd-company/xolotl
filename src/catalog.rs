//! Model catalog: which models each provider serves, discovered from the
//! providers and merged with static entries from the config, plus which wire
//! formats (Anthropic Messages, OpenAI Responses, OpenAI Chat) a model speaks
//! natively, and how every LLM endpoint reaches it: natively, or through
//! translation (`src/translate`) from a format it speaks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::{CatalogConfig, ProviderKind};
use crate::gateway::Gateway;
use crate::policy::Catalog;
use crate::subscription::{SubProvider, Subscriptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// Anthropic `/v1/messages`.
    Messages,
    /// OpenAI `/v1/responses`.
    Responses,
    /// OpenAI `/v1/chat/completions`.
    Chat,
}

impl Wire {
    /// The wire format of an LLM endpoint; `None` for other endpoints
    /// (embeddings, OCR, ...), which are not format-checked.
    pub fn for_path(path: &str) -> Option<Wire> {
        match path {
            "/v1/messages" | "/v1/messages/count_tokens" => Some(Wire::Messages),
            "/v1/responses" | "/v1/responses/compact" => Some(Wire::Responses),
            "/v1/chat/completions" => Some(Wire::Chat),
            _ => None,
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Wire::Messages => "/v1/messages",
            Wire::Responses => "/v1/responses",
            Wire::Chat => "/v1/chat/completions",
        }
    }

    pub const ALL: [Wire; 3] = [Wire::Messages, Wire::Responses, Wire::Chat];
}

/// How a model is reached on an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The provider speaks the endpoint's format.
    Native,
    /// The request and response are translated to and from this format.
    Translated(Wire),
}

/// The format a translated request is sent in, best first: Responses and
/// Messages carry reasoning between turns, Chat does not.
const TRANSLATION_PREFERENCE: [Wire; 3] = [Wire::Responses, Wire::Messages, Wire::Chat];

/// OpenCode Go serves each model over one format (from its documentation);
/// anything not listed uses chat completions.
const OPENCODE_GO_MESSAGES: &[&str] = &["claude-haiku-5-5", "minimax-m3", "minimax-m2.7", "qwen3.8-max", "qwen3.8-flash", "qwen3.7-plus"];
const OPENCODE_GO_RESPONSES: &[&str] =
    &["grok-4.7", "grok-4.6", "gpt-6-luna", "gpt-5.6-luna", "muse-spark-1.3-contributor", "muse-spark-1.2-contributor"];

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscoveryStatus {
    pub refreshed_at: Option<i64>,
    pub models: usize,
    pub error: Option<String>,
}

#[derive(Default)]
struct Snapshot {
    models: Catalog,
    refreshed: Option<Instant>,
    discovery: BTreeMap<String, DiscoveryStatus>,
}

pub struct ModelCatalog {
    refresh_lock: tokio::sync::Mutex<()>,
    cfg: CatalogConfig,
    /// Gateway provider name -> kind.
    gateway_kinds: BTreeMap<String, ProviderKind>,
    snapshot: RwLock<Arc<Snapshot>>,
    metadata: RwLock<crate::metadata::Registry>,
    codex_entries: RwLock<BTreeMap<String, serde_json::Value>>,
    db: Option<crate::db::Db>,
}

impl ModelCatalog {
    pub fn new(cfg: CatalogConfig, gateway_kinds: BTreeMap<String, ProviderKind>) -> Self {
        let me = Self {
            refresh_lock: tokio::sync::Mutex::new(()),
            cfg,
            gateway_kinds,
            metadata: Default::default(),
            codex_entries: Default::default(),
            db: None,
            snapshot: RwLock::new(Arc::new(Snapshot::default())),
        };
        // Start with the static entries so routing works before discovery.
        let models = me.with_static(Catalog::new());
        *me.snapshot.write() = Arc::new(Snapshot { models, ..Default::default() });
        me
    }

    pub async fn with_db(mut self, db: crate::db::Db) -> Self {
        if let Ok(Some(body)) = db.metadata_cache().await
            && let Ok(cached) = serde_json::from_str::<crate::metadata::Registry>(&body)
            && cached.entries.values().all(|m| m.validate().is_ok())
        {
            *self.metadata.write() = cached;
        }
        self.db = Some(db);
        self
    }

    pub fn metadata(&self, provider: &str, model: &str) -> crate::metadata::Resolved {
        self.metadata.read().lookup(provider, model, self.cfg.metadata.get(&format!("{provider}/{model}")))
    }

    pub async fn refresh_metadata(&self, http: &reqwest::Client) {
        let _guard = self.refresh_lock.lock().await;
        self.refresh_metadata_unlocked(http).await;
    }

    async fn refresh_metadata_unlocked(&self, http: &reqwest::Client) {
        if self.cfg.metadata_url.is_empty() {
            return;
        }
        let result: anyhow::Result<crate::metadata::Registry> = async {
            let resp = http.get(&self.cfg.metadata_url).send().await?.error_for_status()?;
            crate::metadata::Registry::parse(resp.json().await?, crate::db::now())
        }
        .await;
        match result {
            Ok(registry) => {
                if let Some(db) = &self.db
                    && let Err(e) = db.save_metadata_cache(&serde_json::to_string(&registry).expect("valid registry")).await
                {
                    tracing::warn!(error = %e, "metadata cache persistence failed");
                }
                *self.metadata.write() = registry;
            }
            Err(e) => self.metadata.write().error = Some(format!("{e:#}")),
        }
    }

    /// Every provider name: subscription pools and gateway providers.
    pub fn known_providers(&self) -> BTreeSet<String> {
        SubProvider::ALL.iter().map(|p| p.as_str().to_string()).chain(self.gateway_kinds.keys().cloned()).collect()
    }

    pub fn models(&self) -> Catalog {
        self.snapshot.read().models.clone()
    }

    pub fn last_refresh(&self) -> Option<Instant> {
        self.snapshot.read().refreshed
    }

    fn with_static(&self, mut c: Catalog) -> Catalog {
        for p in self.known_providers() {
            c.entry(p).or_default();
        }
        for (p, models) in &self.cfg.static_models {
            c.entry(p.clone()).or_default().extend(models.iter().cloned());
        }
        c
    }

    /// Discovery results include failed attempts even when cached or static models remain.
    pub fn discovery_status(&self) -> BTreeMap<String, DiscoveryStatus> {
        let snapshot = self.snapshot.read();
        snapshot
            .models
            .iter()
            .map(|(provider, models)| {
                let mut status = snapshot.discovery.get(provider).cloned().unwrap_or_default();
                status.models = models.len();
                (provider.clone(), status)
            })
            .collect()
    }

    /// Re-discover all providers. Providers that fail keep their previous list.
    pub async fn refresh(&self, subs: &Subscriptions, gateway: &Gateway) {
        self.refresh_providers(subs, gateway, &self.known_providers()).await;
    }

    /// Serialize refreshes so a scoped refresh cannot overwrite another provider's result.
    pub async fn refresh_providers(&self, subs: &Subscriptions, gateway: &Gateway, providers: &BTreeSet<String>) {
        let _guard = self.refresh_lock.lock().await;
        self.refresh_metadata_unlocked(subs.http()).await;
        let mut fresh = self.models();
        let mut discovery = self.snapshot.read().discovery.clone();
        for name in providers {
            let result = match name.parse::<SubProvider>() {
                Ok(SubProvider::Codex) => match subs.fetch_codex_catalog(&self.cfg.codex_client_version).await {
                    Ok(body) => {
                        let names = crate::subscription::codex::parse_models(&body);
                        let entries = body["models"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|m| Some((m["slug"].as_str()?.to_string(), m.clone())))
                            .collect();
                        *self.codex_entries.write() = entries;
                        Ok(names)
                    }
                    Err(e) => Err(e),
                },
                Ok(provider) => subs.fetch_models(provider, &self.cfg.codex_client_version).await,
                Err(_) => gateway.fetch_models(name).await,
            };
            let error = match result {
                Ok(models) => {
                    fresh.insert(name.clone(), models.into_iter().collect());
                    None
                }
                Err(e) => {
                    tracing::debug!(provider = %name, error = %e, "model discovery failed");
                    Some(format!("{e:#}"))
                }
            };
            discovery.insert(name.clone(), DiscoveryStatus { refreshed_at: Some(crate::db::now()), error, models: 0 });
        }
        let models = self.with_static(fresh);
        let total: usize = models.values().map(BTreeSet::len).sum();
        tracing::info!(models = total, "model catalog refreshed");
        *self.snapshot.write() = Arc::new(Snapshot { models, discovery, refreshed: Some(Instant::now()) });
    }

    /// Native Codex fields are retained; only the policy-visible identity changes.
    pub fn codex_entry(&self, name: &str, targets: &[crate::policy::Target]) -> serde_json::Value {
        use serde_json::json;
        let first = &targets[0];
        let metadata = self.metadata(&first.provider, &first.model);
        let override_ = self.cfg.metadata.get(&first.qualified());
        let mut entry = json!({
            "shell_type": "disabled", "visibility": "list", "supported_in_api": true, "priority": 0,
            "supported_reasoning_levels": [], "support_verbosity": false, "default_verbosity": null,
            "availability_nux": null, "upgrade": null, "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10000}, "experimental_supported_tools": [],
            "input_modalities": ["text"], "supports_reasoning_summaries": false,
            "supports_reasoning_summary_parameter": false, "parallel_tool_calls": false,
            "base_instructions": "You are a coding assistant.", "effective_context_window_percent": 95
        });
        if first.provider == "codex"
            && let Some(native) = self.codex_entries.read().get(&first.model).and_then(serde_json::Value::as_object)
        {
            entry.as_object_mut().unwrap().extend(native.clone());
        }
        entry["slug"] = json!(name);
        entry["display_name"] = json!(name);
        entry["description"] = json!(metadata.description(&first.provider));
        // An alias's context must be safe for every fallback target. Unknown is unknown.
        let limits: Option<Vec<u64>> = targets
            .iter()
            .map(|t| {
                let configured = self.cfg.metadata.get(&t.qualified()).and_then(|o| o.values.max_input_tokens);
                let native = if t.provider == "codex" {
                    self.codex_entries.read().get(&t.model).and_then(|m| m["context_window"].as_u64())
                } else {
                    None
                };
                configured.or(native).or(self.metadata(&t.provider, &t.model).values.max_input_tokens)
            })
            .collect();
        if let Some(window) = limits.and_then(|xs| xs.into_iter().min()) {
            entry["context_window"] = json!(window);
            entry["max_context_window"] = json!(window);
        } else {
            for key in ["context_window", "max_context_window", "auto_compact_token_limit"] {
                entry.as_object_mut().unwrap().remove(key);
            }
        }
        if let Some(max) = metadata.values.max_output_tokens {
            entry["max_output_tokens"] = json!(max);
        }
        if let Some(o) = override_ {
            if let Some(images) = o.images {
                entry["input_modalities"] = if images { json!(["text", "image"]) } else { json!(["text"]) };
            }
            if let Some(tools) = o.tools {
                entry["shell_type"] = json!(if tools { "unified_exec" } else { "disabled" });
            }
            if let Some(levels) = &o.reasoning_levels {
                entry["supported_reasoning_levels"] =
                    json!(levels.iter().map(|l| json!({"effort": l, "description": l})).collect::<Vec<_>>());
                entry["default_reasoning_level"] = levels.first().map(|l| json!(l)).unwrap_or(serde_json::Value::Null);
            }
        }
        if targets.len() > 1 {
            let entries: Vec<_> = targets.iter().map(|t| self.codex_entry(name, std::slice::from_ref(t))).collect();
            if entries.iter().any(|m| m["shell_type"] != "unified_exec") {
                entry["shell_type"] = json!("disabled");
            }
            if entries.iter().any(|m| !m["input_modalities"].as_array().is_some_and(|xs| xs.contains(&json!("image")))) {
                entry["input_modalities"] = json!(["text"]);
            }
            let common: Vec<_> = entries[0]["supported_reasoning_levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|level| {
                    entries.iter().all(|m| {
                        m["supported_reasoning_levels"].as_array().is_some_and(|xs| xs.iter().any(|x| x["effort"] == level["effort"]))
                    })
                })
                .cloned()
                .collect();
            entry["default_reasoning_level"] = common.first().map(|v| v["effort"].clone()).unwrap_or(serde_json::Value::Null);
            entry["supported_reasoning_levels"] = json!(common);
        }
        entry
    }

    /// Wire formats that reach `provider/model`.
    pub fn wires(&self, provider: &str, model: &str) -> Vec<Wire> {
        // Claude subscription accounts speak only Messages; overrides don't apply.
        if provider == SubProvider::Claude.as_str() {
            return vec![Wire::Messages];
        }
        if let Some(w) = self.cfg.wires.get(&format!("{provider}/{model}")).or_else(|| self.cfg.wires.get(provider)) {
            return w.clone();
        }
        match provider {
            "codex" | "grok" => vec![Wire::Responses],
            "opencode-go" => {
                if OPENCODE_GO_MESSAGES.contains(&model) {
                    vec![Wire::Messages]
                } else if OPENCODE_GO_RESPONSES.contains(&model) {
                    vec![Wire::Responses]
                } else {
                    vec![Wire::Chat]
                }
            }
            other => match self.gateway_kinds.get(other) {
                Some(ProviderKind::Vercel) => vec![Wire::Chat, Wire::Responses, Wire::Messages],
                _ => vec![Wire::Chat],
            },
        }
    }

    /// How `provider/model` is reached on `path`, an endpoint of format
    /// `wire`. A native format always wins. Only the main endpoint of each
    /// format is translated, so `count_tokens` and `compact` are native only.
    /// Claude subscription accounts are never translated: they serve only the
    /// Claude Code API path. Anthropic models on other providers are.
    pub fn route(&self, provider: &str, model: &str, wire: Wire, path: &str) -> Option<Route> {
        if provider == SubProvider::Claude.as_str() {
            return (wire == Wire::Messages).then_some(Route::Native);
        }
        let native = self.wires(provider, model);
        if native.contains(&wire) {
            return Some(Route::Native);
        }
        if path != wire.path() {
            return None;
        }
        TRANSLATION_PREFERENCE.into_iter().find(|w| native.contains(w)).map(Route::Translated)
    }

    /// The main endpoints that reach `provider/model`: (native, translated).
    pub fn endpoints(&self, provider: &str, model: &str) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut native = Vec::new();
        let mut translated = Vec::new();
        for w in Wire::ALL {
            match self.route(provider, model, w, w.path()) {
                Some(Route::Native) => native.push(w.path()),
                Some(Route::Translated(_)) => translated.push(w.path()),
                None => {}
            }
        }
        (native, translated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_rules_and_overrides() {
        let mut cfg = CatalogConfig::default();
        cfg.wires.insert("llamacpp".into(), vec![Wire::Chat, Wire::Messages]);
        cfg.static_models.insert("codex".into(), vec!["gpt-6-codex".into()]);
        let kinds =
            BTreeMap::from([("vercel".to_string(), ProviderKind::Vercel), ("llamacpp".to_string(), ProviderKind::OpenaiCompatible)]);
        let c = ModelCatalog::new(cfg, kinds);
        assert_eq!(c.wires("claude", "x"), vec![Wire::Messages]);
        assert_eq!(c.wires("opencode-go", "minimax-m3"), vec![Wire::Messages]);
        assert_eq!(c.wires("opencode-go", "kimi-k3"), vec![Wire::Chat]);
        assert_eq!(c.wires("grok", "grok-build"), vec![Wire::Responses]);
        assert!(c.wires("vercel", "a/b").contains(&Wire::Messages));
        assert_eq!(c.wires("llamacpp", "q"), vec![Wire::Chat, Wire::Messages]);
        let models = c.models();
        assert!(models["codex"].contains("gpt-6-codex"));
        assert!(models.contains_key("vercel") && models.contains_key("opencode-go"));
        assert_eq!(Wire::for_path("/v1/messages/count_tokens"), Some(Wire::Messages));
        assert_eq!(Wire::for_path("/v1/embeddings"), None);
    }

    #[test]
    fn claude_accounts_ignore_wire_overrides() {
        let mut cfg = CatalogConfig::default();
        cfg.wires.insert("claude".into(), vec![Wire::Chat, Wire::Responses]);
        cfg.wires.insert("claude/m".into(), vec![Wire::Chat]);
        let c = ModelCatalog::new(cfg, BTreeMap::new());
        for model in ["m", "other"] {
            assert_eq!(c.wires("claude", model), vec![Wire::Messages]);
            assert_eq!(c.route("claude", model, Wire::Messages, "/v1/messages"), Some(Route::Native));
            assert_eq!(c.route("claude", model, Wire::Chat, "/v1/chat/completions"), None);
            assert_eq!(c.route("claude", model, Wire::Responses, "/v1/responses"), None);
            assert_eq!(c.endpoints("claude", model), (vec!["/v1/messages"], vec![]));
        }
    }

    #[test]
    fn native_routes_win_and_translation_prefers_responses() {
        let mut cfg = CatalogConfig::default();
        cfg.wires.insert("both".into(), vec![Wire::Chat, Wire::Messages]);
        let kinds = BTreeMap::from([
            ("vercel".to_string(), ProviderKind::Vercel),
            ("llamacpp".to_string(), ProviderKind::OpenaiCompatible),
            ("both".to_string(), ProviderKind::OpenaiCompatible),
        ]);
        let c = ModelCatalog::new(cfg, kinds);
        assert_eq!(c.route("claude", "m", Wire::Messages, "/v1/messages"), Some(Route::Native));
        // Claude subscription accounts stay on the Claude Code path; other Messages providers translate.
        assert_eq!(c.route("claude", "m", Wire::Chat, "/v1/chat/completions"), None);
        assert_eq!(c.route("claude", "m", Wire::Responses, "/v1/responses"), None);
        assert_eq!(c.endpoints("claude", "m"), (vec!["/v1/messages"], vec![]));
        assert_eq!(c.route("opencode-go", "minimax-m3", Wire::Chat, "/v1/chat/completions"), Some(Route::Translated(Wire::Messages)));
        assert_eq!(c.route("codex", "m", Wire::Messages, "/v1/messages"), Some(Route::Translated(Wire::Responses)));
        assert_eq!(c.route("llamacpp", "m", Wire::Responses, "/v1/responses"), Some(Route::Translated(Wire::Chat)));
        assert_eq!(c.route("both", "m", Wire::Responses, "/v1/responses"), Some(Route::Translated(Wire::Messages)));
        assert_eq!(c.route("vercel", "a/b", Wire::Messages, "/v1/messages"), Some(Route::Native));
        // Endpoints other than the three main ones are never translated.
        assert_eq!(c.route("codex", "m", Wire::Messages, "/v1/messages/count_tokens"), None);
        assert_eq!(c.route("claude", "m", Wire::Responses, "/v1/responses/compact"), None);
        assert_eq!(c.route("claude", "m", Wire::Messages, "/v1/messages/count_tokens"), Some(Route::Native));
        assert_eq!(c.endpoints("codex", "m"), (vec!["/v1/responses"], vec!["/v1/messages", "/v1/chat/completions"]));
    }
}
