//! Client key policies: which providers and models a key may use, how model
//! names served by several providers resolve, and aliases.
//!
//! A plain model name served by more than one allowed provider belongs to a
//! single provider: the per-model `winners` entry if present, otherwise the
//! first provider in `precedence`, otherwise the built-in default order
//! (subscription pools first, then gateway providers alphabetically). The
//! other providers drop that plain name, but the explicit `provider/model`
//! form always works for any model the policy allows, and aliases can expose
//! a losing provider's model under any name.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::keys::{Scope, glob_match};

/// Default precedence before any gateway providers.
pub const DEFAULT_PRECEDENCE: &[&str] = &["claude", "codex", "grok", "opencode-go"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Provider names (`claude`, `codex`, `grok`, `opencode-go`, gateway provider names); `*` allows all.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Globs matched against `model` or `provider/model`. Empty allows all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_models: Vec<String>,
    /// Globs that remove models; deny wins over allow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_models: Vec<String>,
    /// Provider order for model names served by several providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub precedence: Vec<String>,
    /// Per-model provider overrides for collisions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub winners: BTreeMap<String, String>,
    /// Client-visible name -> `provider/model` target(s), tried in order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, AliasTargets>,
    /// What keys under this policy may do. Defaults to `proxy`.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<Scope>,
    /// MCP servers (names from `[mcp.servers]`) keys may use through `/mcp`; `*` allows all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    /// Globs matched against `tool` or `server/tool`. Empty allows every tool
    /// of the allowed servers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    /// Globs that remove tools; deny wins over allow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
}

fn default_scopes() -> Vec<Scope> {
    vec![Scope::Proxy]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AliasTargets {
    One(String),
    Many(Vec<String>),
}

impl AliasTargets {
    pub fn list(&self) -> Vec<&str> {
        match self {
            AliasTargets::One(s) => vec![s.as_str()],
            AliasTargets::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Target {
    pub provider: String,
    pub model: String,
}

impl Target {
    pub fn parse(s: &str) -> Option<Self> {
        let (p, m) = s.split_once('/')?;
        (!p.is_empty() && !m.is_empty()).then(|| Target { provider: p.to_string(), model: m.to_string() })
    }
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Provider name -> models it currently serves.
pub type Catalog = BTreeMap<String, BTreeSet<String>>;

/// A model name several providers serve.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SharedName {
    pub model: String,
    /// In precedence order.
    pub providers: Vec<String>,
    pub winner: String,
}

/// The model names a policy exposes for a catalog.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    /// Plain names and aliases -> targets (aliases may have fallbacks).
    pub names: BTreeMap<String, Vec<Target>>,
}

fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

impl Policy {
    /// The implicit policy of the bootstrap admin token: everything allowed.
    pub fn unrestricted() -> Self {
        Policy {
            name: "_admin".into(),
            providers: vec!["*".into()],
            scopes: vec![Scope::Admin],
            mcp_servers: vec!["*".into()],
            ..Default::default()
        }
    }

    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        self.description = self.description.trim().to_string();
        let lower = |v: &mut Vec<String>| {
            let mut seen = BTreeSet::new();
            v.retain_mut(|s| {
                *s = s.trim().to_ascii_lowercase();
                !s.is_empty() && seen.insert(s.clone())
            });
        };
        lower(&mut self.providers);
        lower(&mut self.precedence);
        lower(&mut self.mcp_servers);
        for v in [&mut self.allow_models, &mut self.deny_models, &mut self.allow_tools, &mut self.deny_tools] {
            v.retain_mut(|s| {
                *s = s.trim().to_string();
                !s.is_empty()
            });
        }
        if self.scopes.is_empty() {
            self.scopes = default_scopes();
        }
    }

    /// `known` lists every provider name the server knows, `mcp` every
    /// configured MCP server.
    pub fn validate(&self, known: &BTreeSet<String>, mcp: &BTreeSet<String>) -> Result<()> {
        anyhow::ensure!(valid_name(&self.name), "policy name must be lowercase letters, digits, '-' or '_' (max 63)");
        anyhow::ensure!(
            !self.providers.is_empty() || !self.mcp_servers.is_empty(),
            "policy must allow at least one provider or MCP server (use * for all)"
        );
        for s in &self.mcp_servers {
            anyhow::ensure!(s == "*" || mcp.contains(s), "unknown MCP server {s:?}");
        }
        anyhow::ensure!(self.mcp_servers.is_empty() || self.has_scope(Scope::Mcp), "mcp_servers needs the mcp scope");
        for p in self.providers.iter().chain(&self.precedence).chain(self.winners.values()) {
            anyhow::ensure!(p == "*" || known.contains(p), "unknown provider {p:?}");
        }
        for (model, p) in &self.winners {
            anyhow::ensure!(self.allows_provider(p), "winner for {model:?} is {p:?}, which the policy does not allow");
        }
        for (name, targets) in &self.aliases {
            anyhow::ensure!(!name.is_empty() && !name.chars().any(char::is_whitespace), "alias {name:?} is not a valid name");
            anyhow::ensure!(!targets.list().is_empty(), "alias {name:?} has no targets");
            for t in targets.list() {
                let target = Target::parse(t).ok_or_else(|| anyhow::anyhow!("alias {name:?}: target {t:?} must be provider/model"))?;
                anyhow::ensure!(known.contains(&target.provider), "alias {name:?}: unknown provider {:?}", target.provider);
                anyhow::ensure!(
                    self.allows_provider(&target.provider),
                    "alias {name:?} targets provider {:?}, which the policy does not allow",
                    target.provider
                );
            }
        }
        Ok(())
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&scope)
    }

    pub fn allows_provider(&self, provider: &str) -> bool {
        self.providers.iter().any(|p| p == "*" || p == provider)
    }

    pub fn allows_model(&self, provider: &str, model: &str) -> bool {
        if !self.allows_provider(provider) {
            return false;
        }
        let qualified = format!("{provider}/{model}");
        let hit = |patterns: &[String]| patterns.iter().any(|p| glob_match(p, model) || glob_match(p, &qualified));
        if hit(&self.deny_models) {
            return false;
        }
        self.allow_models.is_empty() || hit(&self.allow_models)
    }

    pub fn allows_mcp_server(&self, server: &str) -> bool {
        self.mcp_servers.iter().any(|s| s == "*" || s == server)
    }

    pub fn allows_tool(&self, server: &str, tool: &str) -> bool {
        if !self.allows_mcp_server(server) {
            return false;
        }
        let qualified = format!("{server}/{tool}");
        let hit = |patterns: &[String]| patterns.iter().any(|p| glob_match(p, tool) || glob_match(p, &qualified));
        if hit(&self.deny_tools) {
            return false;
        }
        self.allow_tools.is_empty() || hit(&self.allow_tools)
    }

    fn rank(&self, provider: &str) -> (usize, usize, String) {
        if let Some(i) = self.precedence.iter().position(|p| p == provider) {
            return (0, i, String::new());
        }
        if let Some(i) = DEFAULT_PRECEDENCE.iter().position(|p| *p == provider) {
            return (1, i, String::new());
        }
        (2, 0, provider.to_string())
    }

    /// Every allowed model -> the allowed providers serving it, in precedence order.
    fn by_model<'a>(&self, catalog: &'a Catalog) -> BTreeMap<&'a str, Vec<&'a str>> {
        let mut by_model: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (provider, models) in catalog {
            for m in models {
                if self.allows_model(provider, m) {
                    by_model.entry(m.as_str()).or_default().push(provider.as_str());
                }
            }
        }
        for providers in by_model.values_mut() {
            providers.sort_by_key(|p| self.rank(p));
        }
        by_model
    }

    /// The provider that serves a plain model name: its `winners` entry, else the first by precedence.
    fn winner<'a>(&self, model: &str, providers: &[&'a str]) -> &'a str {
        self.winners.get(model).and_then(|w| providers.iter().find(|p| **p == w.as_str())).copied().unwrap_or(providers[0])
    }

    /// Model names that several allowed providers serve, and which one wins each.
    pub fn shared_names(&self, catalog: &Catalog) -> Vec<SharedName> {
        self.by_model(catalog)
            .into_iter()
            .filter(|(_, providers)| providers.len() > 1)
            .map(|(model, providers)| SharedName {
                model: model.to_string(),
                winner: self.winner(model, &providers).to_string(),
                providers: providers.into_iter().map(str::to_string).collect(),
            })
            .collect()
    }

    /// Resolve the client-visible model names for a catalog.
    pub fn resolve(&self, catalog: &Catalog) -> View {
        let mut view = View::default();
        for (model, providers) in self.by_model(catalog) {
            let winner = self.winner(model, &providers);
            view.names.insert(model.to_string(), vec![Target { provider: winner.into(), model: model.into() }]);
        }
        // Aliases are explicit grants and shadow catalog names.
        for (name, targets) in &self.aliases {
            let ts: Vec<Target> =
                targets.list().into_iter().filter_map(Target::parse).filter(|t| self.allows_provider(&t.provider)).collect();
            if !ts.is_empty() {
                view.names.insert(name.clone(), ts);
            }
        }
        view
    }
}

impl View {
    /// Resolve a requested model: an exposed name, or the explicit
    /// `provider/model` form for any model the policy allows.
    pub fn lookup(&self, policy: &Policy, known: &BTreeSet<String>, requested: &str) -> Option<Vec<Target>> {
        if let Some(t) = self.names.get(requested) {
            return Some(t.clone());
        }
        let t = Target::parse(requested)?;
        (known.contains(&t.provider) && policy.allows_model(&t.provider, &t.model)).then(|| vec![t])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let mut c = Catalog::new();
        c.insert("claude".into(), ["claude-haiku-5-5", "claude-opus-5"].iter().map(|s| s.to_string()).collect());
        c.insert("codex".into(), ["gpt-6-codex"].iter().map(|s| s.to_string()).collect());
        c.insert("opencode-go".into(), ["claude-haiku-5-5", "kimi-k3", "minimax-m3"].iter().map(|s| s.to_string()).collect());
        c.insert("vercel".into(), ["anthropic/claude-opus-5", "kimi-k3"].iter().map(|s| s.to_string()).collect());
        c
    }

    fn known() -> BTreeSet<String> {
        catalog().keys().cloned().collect()
    }

    fn policy() -> Policy {
        Policy { name: "dev".into(), providers: vec!["*".into()], ..Default::default() }
    }

    #[test]
    fn default_precedence_prefers_subscriptions() {
        let v = policy().resolve(&catalog());
        assert_eq!(v.names["claude-haiku-5-5"][0].provider, "claude");
        assert_eq!(v.names["kimi-k3"][0].provider, "opencode-go", "subscription pool beats gateway");
        assert_eq!(v.names["minimax-m3"][0].provider, "opencode-go");
        assert_eq!(v.names["anthropic/claude-opus-5"][0].provider, "vercel");
    }

    #[test]
    fn precedence_and_winners() {
        let mut p = policy();
        p.precedence = vec!["vercel".into(), "opencode-go".into()];
        p.winners.insert("claude-haiku-5-5".into(), "opencode-go".into());
        let v = p.resolve(&catalog());
        assert_eq!(v.names["kimi-k3"][0].provider, "vercel");
        assert_eq!(v.names["claude-haiku-5-5"][0].provider, "opencode-go");
        assert_eq!(v.names["claude-opus-5"][0].provider, "claude", "unlisted providers keep default order");
        let shared = p.shared_names(&catalog());
        let haiku = shared.iter().find(|s| s.model == "claude-haiku-5-5").unwrap();
        assert_eq!(haiku.providers, ["opencode-go", "claude"]);
        assert_eq!(haiku.winner, "opencode-go");
        assert!(shared.iter().all(|s| s.model != "claude-opus-5"), "one provider is not shared");
    }

    #[test]
    fn explicit_prefix_reaches_losers_and_aliases_rename() {
        let mut p = policy();
        p.aliases.insert("haiku-go".into(), AliasTargets::One("opencode-go/claude-haiku-5-5".into()));
        p.aliases.insert("fast".into(), AliasTargets::Many(vec!["codex/gpt-6-codex".into(), "vercel/kimi-k3".into()]));
        let v = p.resolve(&catalog());
        let k = known();
        assert_eq!(v.lookup(&p, &k, "opencode-go/claude-haiku-5-5").unwrap()[0].provider, "opencode-go");
        assert_eq!(v.lookup(&p, &k, "haiku-go").unwrap()[0].model, "claude-haiku-5-5");
        assert_eq!(v.lookup(&p, &k, "fast").unwrap().len(), 2);
        // Explicit form works for models missing from the catalog, if allowed.
        assert!(v.lookup(&p, &k, "vercel/openai/gpt-6").is_some());
        assert!(v.lookup(&p, &k, "nope/x").is_none());
        assert!(v.lookup(&p, &k, "unknown-model").is_none());
    }

    #[test]
    fn allow_deny_and_providers() {
        let mut p = policy();
        p.providers = vec!["claude".into(), "opencode-go".into()];
        p.deny_models = vec!["opencode-go/*".into()];
        p.allow_models = vec!["claude-*".into(), "kimi-*".into()];
        let v = p.resolve(&catalog());
        assert_eq!(v.names.keys().cloned().collect::<Vec<_>>(), vec!["claude-haiku-5-5", "claude-opus-5"]);
        assert!(v.lookup(&p, &known(), "opencode-go/kimi-k3").is_none(), "deny wins");
        assert!(v.lookup(&p, &known(), "codex/gpt-6-codex").is_none(), "provider not allowed");
    }

    #[test]
    fn validation() {
        let k = known();
        let mut p = policy();
        p.normalize();
        p.validate(&k, &BTreeSet::new()).unwrap();
        p.name = "Bad Name".into();
        assert!(p.validate(&k, &BTreeSet::new()).is_err());
        let mut p = policy();
        p.providers = vec!["claude".into()];
        p.aliases.insert("x".into(), AliasTargets::One("codex/gpt".into()));
        assert!(p.validate(&k, &BTreeSet::new()).is_err(), "alias to disallowed provider");
        let mut p = policy();
        p.precedence = vec!["nope".into()];
        assert!(p.validate(&k, &BTreeSet::new()).is_err());
        let parsed: Policy = serde_json::from_str(r#"{"name":"a","providers":["*"]}"#).unwrap();
        assert_eq!(parsed.scopes, vec![Scope::Proxy]);
    }

    #[test]
    fn mcp_servers_and_tools() {
        let mcp: BTreeSet<String> = ["github-work", "linear"].iter().map(|s| s.to_string()).collect();
        let mut p: Policy = serde_json::from_str(r#"{"name":"agents","scopes":["mcp"],"mcp_servers":["github-work"]}"#).unwrap();
        p.normalize();
        p.validate(&known(), &mcp).unwrap();
        p.deny_tools = vec!["delete_*".into()];
        p.allow_tools = vec!["github-work/*".into()];
        assert!(p.allows_tool("github-work", "create_issue"));
        assert!(!p.allows_tool("github-work", "delete_repo"), "deny wins");
        assert!(!p.allows_tool("linear", "list_issues"), "server not allowed");
        assert!(!p.allows_model("claude", "claude-opus-5"), "no providers");

        p.mcp_servers = vec!["nope".into()];
        assert!(p.validate(&known(), &mcp).is_err(), "unknown server");
        p.mcp_servers = vec!["linear".into()];
        p.scopes = vec![Scope::Proxy];
        assert!(p.validate(&known(), &mcp).unwrap_err().to_string().contains("mcp scope"));
        p.mcp_servers.clear();
        assert!(p.validate(&known(), &mcp).is_err(), "nothing allowed");
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    #[test]
    fn example_policy_parses_and_validates() {
        let raw = include_str!("../examples/policies/devcontainer.toml");
        let mut p: Policy = toml::from_str(raw).unwrap();
        p.normalize();
        let known: BTreeSet<String> = ["claude", "codex", "grok", "opencode-go", "vercel"].iter().map(|s| s.to_string()).collect();
        p.validate(&known, &BTreeSet::new()).unwrap();
        assert_eq!(p.aliases["fast"].list().len(), 2);
    }
}
