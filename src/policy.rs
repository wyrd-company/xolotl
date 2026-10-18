//! Client key policies: which providers and models a key may use, how model
//! names served by several providers resolve, and aliases.
//!
//! A plain model name served by more than one allowed provider belongs to a
//! single provider: the per-model `winners` entry if present, otherwise the
//! first provider in `precedence`, otherwise the built-in default order
//! (subscription pools first, then gateway providers alphabetically). The
//! other providers drop that plain name, but the explicit `provider/model`
//! form always works for any model the policy allows, and aliases can expose
//! a losing provider's model under any name. A renamed model is offered under
//! its new name instead of its plain name, and leaves the plain name to the
//! other providers.
//!
//! For a subscription provider, `accounts` names the accounts of its pool
//! that serve the policy's keys. A provider without an entry has none.
//!
//! Whether a model is allowed: a matching `deny_models` pattern denies it,
//! else a matching `allow_models` pattern allows it, else its `models`
//! toggle decides, else it is allowed when there are no allow patterns.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::keys::{Scope, glob_match};
use crate::limits::Limits;

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
    /// Subscription provider -> the account ids of its pool keys may use; `*`
    /// allows all, also accounts added later. A provider without an entry
    /// has no accounts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Vec<String>>,
    /// Allow patterns: globs matched against `model` or `provider/model`.
    /// They win over `models`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_models: Vec<String>,
    /// Deny patterns; they win over allow patterns and `models`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_models: Vec<String>,
    /// `provider/model` -> on or off, for a model no pattern matches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, bool>,
    /// Provider order for model names served by several providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub precedence: Vec<String>,
    /// Per-model provider overrides for collisions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub winners: BTreeMap<String, String>,
    /// Client-visible name -> `provider/model` target(s), tried in order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, AliasTargets>,
    /// `provider/model` -> the name keys see instead of its plain name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub renames: BTreeMap<String, String>,
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
    /// Rate limits and token budgets of the LLM endpoints.
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
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

/// A catalog model of an allowed provider, and what the policy does with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub allowed: bool,
    /// The `allow_models` / `deny_models` pattern that decided; `None` when
    /// none matched (the model's toggle or the default decided).
    pub rule: Option<String>,
    /// The name keys see this model under: its rename, or its plain name if
    /// it won it. `None` if it is denied, lost its plain name, or its name
    /// was taken (`taken_by`).
    pub name: Option<String>,
    /// What serves the name this model would have had instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_by: Option<TakenBy>,
}

/// What took a model's plain name.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TakenBy {
    /// An alias of the same name.
    Alias,
    /// Another model renamed to it.
    Rename { target: String },
}

/// The model names a policy exposes for a catalog.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    /// Plain names and aliases -> targets (aliases may have fallbacks).
    pub names: BTreeMap<String, Vec<Target>>,
}

fn is_pool(provider: &str) -> bool {
    provider.parse::<crate::subscription::SubProvider>().is_ok()
}

/// The subscription provider of an account id: ids are `<provider>-<suffix>`.
pub fn account_provider(id: &str) -> Option<&'static str> {
    crate::subscription::SubProvider::ALL
        .into_iter()
        .map(|p| p.as_str())
        .find(|p| id.strip_prefix(p).is_some_and(|rest| rest.len() > 1 && rest.starts_with('-')))
}

/// A client-visible name: an alias or a rename.
fn valid_model_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace)
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
            accounts: crate::subscription::SubProvider::ALL.iter().map(|p| (p.as_str().to_string(), vec!["*".into()])).collect(),
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
        self.accounts = std::mem::take(&mut self.accounts)
            .into_iter()
            .map(|(p, mut ids)| {
                let mut seen = BTreeSet::new();
                ids.retain_mut(|s| {
                    *s = s.trim().to_string();
                    !s.is_empty() && seen.insert(s.clone())
                });
                (p.trim().to_ascii_lowercase(), ids)
            })
            .filter(|(_, ids)| !ids.is_empty())
            .collect();
        self.models = std::mem::take(&mut self.models).into_iter().map(|(t, on)| (t.trim().to_string(), on)).collect();
        lower(&mut self.mcp_servers);
        for v in [&mut self.allow_models, &mut self.deny_models, &mut self.allow_tools, &mut self.deny_tools] {
            v.retain_mut(|s| {
                *s = s.trim().to_string();
                !s.is_empty()
            });
        }
        self.renames = std::mem::take(&mut self.renames)
            .into_iter()
            .map(|(t, n)| (t.trim().to_string(), n.trim().to_string()))
            .filter(|(_, n)| !n.is_empty())
            .collect();
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
        self.limits.validate()?;
        for p in self.providers.iter().chain(&self.precedence).chain(self.winners.values()) {
            anyhow::ensure!(p == "*" || known.contains(p), "unknown provider {p:?}");
        }
        for (provider, ids) in &self.accounts {
            anyhow::ensure!(is_pool(provider), "accounts.{provider}: {provider:?} is not a subscription provider");
            anyhow::ensure!(self.allows_provider(provider), "accounts.{provider}: the policy does not allow provider {provider:?}");
            for id in ids {
                anyhow::ensure!(
                    id == "*" || account_provider(id) == Some(provider.as_str()),
                    "accounts.{provider}: {id:?} is not a {provider} account"
                );
            }
        }
        for target in self.models.keys() {
            anyhow::ensure!(Target::parse(target).is_some(), "models: {target:?} must be provider/model");
        }
        for p in self.limits.providers.keys() {
            anyhow::ensure!(known.contains(p), "limits.providers: unknown provider {p:?}");
            anyhow::ensure!(self.allows_provider(p), "limits.providers: the policy does not allow provider {p:?}");
        }
        for id in self.limits.accounts.keys() {
            let provider = account_provider(id).ok_or_else(|| anyhow::anyhow!("limits.accounts: {id:?} is not an account id"))?;
            anyhow::ensure!(self.allows_account(provider, id), "limits.accounts: the policy does not use account {id:?}");
        }
        for (model, p) in &self.winners {
            anyhow::ensure!(self.allows_provider(p), "winner for {model:?} is {p:?}, which the policy does not allow");
        }
        for (name, targets) in &self.aliases {
            anyhow::ensure!(valid_model_name(name), "alias {name:?} is not a valid name");
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
        let mut renamed = BTreeSet::new();
        for (target, name) in &self.renames {
            let t = Target::parse(target).ok_or_else(|| anyhow::anyhow!("rename {target:?} must be provider/model"))?;
            anyhow::ensure!(known.contains(&t.provider), "rename {target:?}: unknown provider {:?}", t.provider);
            anyhow::ensure!(self.allows_provider(&t.provider), "rename {target:?}: the policy does not allow provider {:?}", t.provider);
            anyhow::ensure!(valid_model_name(name), "rename {target:?}: {name:?} is not a valid name");
            anyhow::ensure!(!self.aliases.contains_key(name), "rename {target:?}: {name:?} is also an alias");
            anyhow::ensure!(renamed.insert(name), "two models are renamed to {name:?}");
        }
        Ok(())
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&Scope::Admin) || self.scopes.contains(&scope)
    }

    pub fn allows_provider(&self, provider: &str) -> bool {
        self.providers.iter().any(|p| p == "*" || p == provider)
    }

    /// Whether keys may use a subscription account of `provider`.
    pub fn allows_account(&self, provider: &str, id: &str) -> bool {
        self.allows_provider(provider) && self.accounts.get(provider).is_some_and(|ids| ids.iter().any(|a| a == "*" || a == id))
    }

    pub fn allows_model(&self, provider: &str, model: &str) -> bool {
        self.allows_provider(provider) && self.model_rule(provider, model).0
    }

    /// Whether the policy allows `provider/model`, and the pattern that
    /// decided: the first matching deny pattern, else the first matching
    /// allow pattern. Without one, the model's toggle decides, else the
    /// absence of allow patterns.
    fn model_rule<'a>(&'a self, provider: &str, model: &str) -> (bool, Option<&'a str>) {
        let qualified = format!("{provider}/{model}");
        let hit = |patterns: &'a [String]| patterns.iter().find(|p| glob_match(p, model) || glob_match(p, &qualified)).map(String::as_str);
        if let Some(p) = hit(&self.deny_models) {
            return (false, Some(p));
        }
        if let Some(p) = hit(&self.allow_models) {
            return (true, Some(p));
        }
        (self.models.get(&qualified).copied().unwrap_or(self.allow_models.is_empty()), None)
    }

    /// The name a rename gives `provider/model`.
    fn rename(&self, provider: &str, model: &str) -> Option<&str> {
        self.renames.get(&format!("{provider}/{model}")).map(String::as_str)
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

    /// Every allowed model that keeps its plain name -> the allowed providers
    /// serving it, in precedence order. Renamed models are left out.
    fn by_model<'a>(&self, catalog: &'a Catalog) -> BTreeMap<&'a str, Vec<&'a str>> {
        let mut by_model: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (provider, models) in catalog {
            for m in models {
                if self.allows_model(provider, m) && self.rename(provider, m).is_none() {
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
        // Renamed models shadow plain names.
        for (provider, models) in catalog {
            for m in models {
                if let Some(name) = self.rename(provider, m).filter(|_| self.allows_model(provider, m)) {
                    view.names.insert(name.to_string(), vec![Target { provider: provider.clone(), model: m.clone() }]);
                }
            }
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

    /// Every catalog model of an allowed provider, allowed or not, with the
    /// name keys see it under in the resolved view.
    pub fn candidates(&self, catalog: &Catalog) -> Vec<Candidate> {
        let view = self.resolve(catalog);
        catalog
            .iter()
            .filter(|(provider, _)| self.allows_provider(provider))
            .flat_map(|(provider, models)| models.iter().map(move |m| (provider, m)))
            .map(|(provider, model)| {
                let (allowed, rule) = self.model_rule(provider, model);
                let me = Target { provider: provider.clone(), model: model.clone() };
                let renamed = self.rename(provider, model);
                // The name this model asks for, and what the view serves under it.
                let wanted = renamed.unwrap_or(model);
                let served = view.names.get(wanted).map(|ts| &ts[0]);
                let taken_by = if !allowed || served == Some(&me) {
                    None
                } else if self.aliases.contains_key(wanted) {
                    Some(TakenBy::Alias)
                } else {
                    // Another model renamed to this name; a plain winner on another provider is not "taken".
                    served.filter(|t| self.rename(&t.provider, &t.model) == Some(wanted)).map(|t| TakenBy::Rename { target: t.qualified() })
                };
                let name = (allowed && served == Some(&me)).then(|| wanted.to_string());
                Candidate { provider: provider.clone(), model: model.clone(), allowed, rule: rule.map(str::to_string), name, taken_by }
            })
            .collect()
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
    fn renames_replace_the_plain_name() {
        let mut p = policy();
        p.renames.insert("claude/claude-haiku-5-5".into(), "haiku".into());
        p.renames.insert("vercel/kimi-k3".into(), "kimi-vercel".into());
        let v = p.resolve(&catalog());
        let k = known();
        assert_eq!(v.names["haiku"], vec![Target { provider: "claude".into(), model: "claude-haiku-5-5".into() }]);
        assert_eq!(v.names["claude-haiku-5-5"][0].provider, "opencode-go", "the plain name goes to the next provider");
        assert_eq!(v.names["kimi-vercel"][0].provider, "vercel");
        assert_eq!(v.names["kimi-k3"][0].provider, "opencode-go");
        assert!(v.lookup(&p, &k, "claude/claude-haiku-5-5").is_some(), "explicit form still works");
        assert!(p.shared_names(&catalog()).iter().all(|s| s.model != "claude-haiku-5-5"), "a renamed model no longer shares the name");

        // A rename overrides a plain name of the same spelling.
        p.renames.insert("opencode-go/minimax-m3".into(), "claude-opus-5".into());
        assert_eq!(p.resolve(&catalog()).names["claude-opus-5"][0].provider, "opencode-go");

        // A denied model is not exposed under its new name.
        p.deny_models = vec!["claude/*".into()];
        assert!(!p.resolve(&catalog()).names.contains_key("haiku"));
    }

    #[test]
    fn candidates_cover_denied_models() {
        let mut p = policy();
        p.providers = vec!["claude".into(), "opencode-go".into()];
        p.deny_models = vec!["opencode-go/kimi-*".into()];
        p.renames.insert("opencode-go/minimax-m3".into(), "minimax".into());
        let c = p.candidates(&catalog());
        let get = |provider: &str, model: &str| c.iter().find(|x| x.provider == provider && x.model == model).unwrap().clone();
        assert_eq!(c.len(), 5, "every model of the allowed providers, none of the others");
        assert_eq!(get("claude", "claude-haiku-5-5").name.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(get("opencode-go", "claude-haiku-5-5").name, None, "lost the plain name");
        assert!(get("opencode-go", "claude-haiku-5-5").allowed);
        let kimi = get("opencode-go", "kimi-k3");
        assert_eq!((kimi.allowed, kimi.rule.as_deref(), kimi.name), (false, Some("opencode-go/kimi-*"), None));
        assert_eq!(get("opencode-go", "minimax-m3").name.as_deref(), Some("minimax"));

        p.deny_models.clear();
        p.allow_models = vec!["claude-*".into()];
        let c = p.candidates(&catalog());
        let minimax = c.iter().find(|x| x.model == "minimax-m3").unwrap();
        assert_eq!((minimax.allowed, minimax.rule.as_deref(), minimax.name.as_deref()), (false, None, None), "not in the allow list");
        let haiku = c.iter().find(|x| x.provider == "claude" && x.model == "claude-haiku-5-5").unwrap();
        assert_eq!(haiku.rule.as_deref(), Some("claude-*"));
    }

    #[test]
    fn candidate_names_follow_shadowing() {
        let mut p = policy();
        p.providers = vec!["codex".into(), "opencode-go".into()];
        // minimax-m3 is renamed onto kimi-k3's plain name; an alias takes gpt-6-codex.
        p.renames.insert("opencode-go/minimax-m3".into(), "kimi-k3".into());
        p.aliases.insert("gpt-6-codex".into(), AliasTargets::One("opencode-go/kimi-k3".into()));
        let c = p.candidates(&catalog());
        let get = |model: &str| c.iter().find(|x| x.model == model).unwrap().clone();
        let v = p.resolve(&catalog());
        assert_eq!(v.names["kimi-k3"][0].model, "minimax-m3", "the rename serves the name");
        assert_eq!(get("minimax-m3").name.as_deref(), Some("kimi-k3"));
        let kimi = get("kimi-k3");
        assert_eq!((kimi.name, kimi.taken_by), (None, Some(TakenBy::Rename { target: "opencode-go/minimax-m3".into() })));
        let codex = get("gpt-6-codex");
        assert_eq!((codex.name, codex.taken_by), (None, Some(TakenBy::Alias)));
        assert_eq!(get("claude-haiku-5-5").taken_by, None);

        // Every reported name is served by its own row.
        for x in &c {
            if let Some(n) = &x.name {
                assert_eq!(v.names[n], vec![Target { provider: x.provider.clone(), model: x.model.clone() }], "{n}");
            }
        }

        // A shared name taken by an alias is taken for the winner and the losers alike.
        let mut p = policy();
        p.aliases.insert("kimi-k3".into(), AliasTargets::One("codex/gpt-6-codex".into()));
        let c = p.candidates(&catalog());
        for provider in ["opencode-go", "vercel"] {
            let k = c.iter().find(|x| x.provider == provider && x.model == "kimi-k3").unwrap();
            assert_eq!((k.name.as_deref(), &k.taken_by), (None, &Some(TakenBy::Alias)), "{provider}");
        }
        let lost = c.iter().find(|x| x.provider == "opencode-go" && x.model == "claude-haiku-5-5").unwrap();
        assert_eq!((lost.name.as_deref(), &lost.taken_by), (None, &None), "lost to a plain winner, not taken");

        // An alias of the plain name that tries this model first keeps the name here.
        let mut p = policy();
        p.aliases.insert("gpt-6-codex".into(), AliasTargets::Many(vec!["codex/gpt-6-codex".into(), "vercel/kimi-k3".into()]));
        let c = p.candidates(&catalog());
        let codex = c.iter().find(|x| x.model == "gpt-6-codex").unwrap();
        assert_eq!((codex.name.as_deref(), &codex.taken_by), (Some("gpt-6-codex"), &None));
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

        let rename = |target: &str, name: &str| {
            let mut p = policy();
            p.providers = vec!["claude".into(), "codex".into()];
            p.renames.insert(target.into(), name.into());
            p
        };
        rename("claude/claude-opus-5", "opus").validate(&k, &BTreeSet::new()).unwrap();
        for (target, name, why) in [
            ("claude-opus-5", "opus", "must be provider/model"),
            ("nope/x", "opus", "unknown provider"),
            ("vercel/kimi-k3", "kimi", "does not allow"),
            ("claude/claude-opus-5", "my opus", "not a valid name"),
        ] {
            let err = rename(target, name).validate(&k, &BTreeSet::new()).unwrap_err().to_string();
            assert!(err.contains(why), "{target} -> {name}: {err}");
        }
        let mut p = rename("claude/claude-opus-5", "opus");
        p.renames.insert("codex/gpt-6-codex".into(), "opus".into());
        assert!(p.validate(&k, &BTreeSet::new()).unwrap_err().to_string().contains("two models"));
        let mut p = rename("claude/claude-opus-5", "opus");
        p.aliases.insert("opus".into(), AliasTargets::One("codex/gpt-6-codex".into()));
        assert!(p.validate(&k, &BTreeSet::new()).unwrap_err().to_string().contains("also an alias"));
        let mut p = rename(" claude/claude-opus-5 ", "  ");
        p.normalize();
        assert!(p.renames.is_empty(), "a blank name removes the rename");
    }

    #[test]
    fn accounts_narrow_the_pools() {
        let k = known();
        let mut p = policy();
        p.providers = vec!["codex".into(), "claude".into(), "vercel".into()];
        assert!(!p.allows_account("codex", "codex-a1"), "no entry: no account");
        p.accounts.insert(" Codex ".into(), vec![" codex-a1 ".into(), "codex-a1".into()]);
        p.accounts.insert("claude".into(), vec!["*".into()]);
        p.accounts.insert("grok".into(), vec![" ".into()]);
        p.normalize();
        assert!(!p.accounts.contains_key("grok"), "an empty list is no entry");
        p.validate(&k, &BTreeSet::new()).unwrap();
        assert_eq!(p.accounts["codex"], ["codex-a1"]);
        assert!(p.allows_account("codex", "codex-a1"));
        assert!(!p.allows_account("codex", "codex-b2"));
        assert!(p.allows_account("claude", "claude-new"), "* takes accounts added later");
        assert!(!p.allows_account("grok", "grok-a1"), "provider not allowed");
        assert_eq!(account_provider("opencode-go-a1"), Some("opencode-go"));
        assert_eq!(account_provider("codex-"), None);

        for (provider, ids, why) in [
            ("vercel", vec!["vercel-a1"], "not a subscription provider"),
            ("grok", vec!["grok-a1"], "does not allow"),
            ("codex", vec!["claude-a1"], "not a codex account"),
        ] {
            let mut bad = p.clone();
            bad.accounts.insert(provider.into(), ids.into_iter().map(str::to_string).collect());
            let err = bad.validate(&k, &BTreeSet::new()).unwrap_err().to_string();
            assert!(err.contains(why), "{provider}: {err}");
        }
    }

    #[test]
    fn patterns_win_over_model_toggles() {
        let mut p = policy();
        p.models.insert("claude/claude-opus-5".into(), false);
        p.models.insert("codex/gpt-6-codex".into(), true);
        assert!(!p.allows_model("claude", "claude-opus-5"), "toggled off");
        assert!(p.allows_model("claude", "claude-haiku-5-5"), "no allow patterns: on by default");
        p.allow_models = vec!["claude-*".into()];
        assert!(p.allows_model("claude", "claude-opus-5"), "the allow pattern wins over the toggle");
        assert!(p.allows_model("codex", "gpt-6-codex"), "toggled on, no pattern");
        assert!(!p.allows_model("opencode-go", "kimi-k3"), "allow patterns: off by default");
        p.deny_models = vec!["*-opus-*".into()];
        assert!(!p.allows_model("claude", "claude-opus-5"), "deny wins");
        let c = p.candidates(&catalog());
        let opus = c.iter().find(|x| x.provider == "claude" && x.model == "claude-opus-5").unwrap();
        assert_eq!((opus.allowed, opus.rule.as_deref()), (false, Some("*-opus-*")));
        let codex = c.iter().find(|x| x.model == "gpt-6-codex").unwrap();
        assert_eq!((codex.allowed, codex.rule.as_deref()), (true, None));
        p.models.insert("nope".into(), true);
        assert!(p.validate(&known(), &BTreeSet::new()).unwrap_err().to_string().contains("provider/model"));
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
