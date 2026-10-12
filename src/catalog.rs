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

#[derive(Default)]
struct Snapshot {
    models: Catalog,
    refreshed: Option<Instant>,
}

pub struct ModelCatalog {
    cfg: CatalogConfig,
    /// Gateway provider name -> kind.
    gateway_kinds: BTreeMap<String, ProviderKind>,
    snapshot: RwLock<Arc<Snapshot>>,
}

impl ModelCatalog {
    pub fn new(cfg: CatalogConfig, gateway_kinds: BTreeMap<String, ProviderKind>) -> Self {
        let me = Self { cfg, gateway_kinds, snapshot: RwLock::new(Arc::new(Snapshot::default())) };
        // Start with the static entries so routing works before discovery.
        let models = me.with_static(Catalog::new());
        *me.snapshot.write() = Arc::new(Snapshot { models, refreshed: None });
        me
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

    /// Re-discover models. Providers that fail keep their previous list.
    pub async fn refresh(&self, subs: &Subscriptions, gateway: &Gateway) {
        let previous = self.models();
        let mut fresh = Catalog::new();
        for p in SubProvider::ALL {
            let models = match subs.fetch_models(p, &self.cfg.codex_client_version).await {
                Ok(m) => m.into_iter().collect(),
                Err(e) => {
                    tracing::debug!(provider = %p, error = %e, "model discovery failed");
                    previous.get(p.as_str()).cloned().unwrap_or_default()
                }
            };
            fresh.insert(p.as_str().to_string(), models);
        }
        for name in self.gateway_kinds.keys() {
            let models = match gateway.fetch_models(name).await {
                Ok(m) => m.into_iter().collect(),
                Err(e) => {
                    tracing::debug!(provider = %name, error = %e, "model discovery failed");
                    previous.get(name).cloned().unwrap_or_default()
                }
            };
            fresh.insert(name.clone(), models);
        }
        let models = self.with_static(fresh);
        let total: usize = models.values().map(BTreeSet::len).sum();
        tracing::info!(models = total, "model catalog refreshed");
        *self.snapshot.write() = Arc::new(Snapshot { models, refreshed: Some(Instant::now()) });
    }

    /// Wire formats that reach `provider/model`.
    pub fn wires(&self, provider: &str, model: &str) -> Vec<Wire> {
        if let Some(w) = self.cfg.wires.get(&format!("{provider}/{model}")).or_else(|| self.cfg.wires.get(provider)) {
            return w.clone();
        }
        match provider {
            "claude" => vec![Wire::Messages],
            "codex" => vec![Wire::Responses],
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
    pub fn route(&self, provider: &str, model: &str, wire: Wire, path: &str) -> Option<Route> {
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
        assert!(c.wires("vercel", "a/b").contains(&Wire::Messages));
        assert_eq!(c.wires("llamacpp", "q"), vec![Wire::Chat, Wire::Messages]);
        let models = c.models();
        assert!(models["codex"].contains("gpt-6-codex"));
        assert!(models.contains_key("vercel") && models.contains_key("opencode-go"));
        assert_eq!(Wire::for_path("/v1/messages/count_tokens"), Some(Wire::Messages));
        assert_eq!(Wire::for_path("/v1/embeddings"), None);
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
        assert_eq!(c.route("claude", "m", Wire::Chat, "/v1/chat/completions"), Some(Route::Translated(Wire::Messages)));
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
