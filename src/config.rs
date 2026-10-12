//! Configuration file (TOML) with environment-variable overrides for the
//! values that commonly differ per deployment.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub vault: VaultConfig,
    pub telemetry: TelemetryConfig,
    pub subscription: SubscriptionConfig,
    pub gateway: GatewayConfig,
    pub catalog: CatalogConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogConfig {
    /// How often to re-discover provider model lists.
    pub refresh_secs: u64,
    /// Codex filters its model roster by client version.
    pub codex_client_version: String,
    /// Extra models per provider, merged with discovered ones.
    pub static_models: BTreeMap<String, Vec<String>>,
    /// Wire-format overrides keyed by `provider` or `provider/model`.
    pub wires: BTreeMap<String, Vec<crate::catalog::Wire>>,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self { refresh_secs: 600, codex_client_version: "0.160.0".into(), static_models: BTreeMap::new(), wires: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Maximum accepted request body in bytes (OCR / batch uploads can be large).
    pub max_body_bytes: usize,
    /// Upstream request timeout in seconds (covers the whole streamed response).
    pub upstream_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8317".parse().unwrap(), max_body_bytes: 64 * 1024 * 1024, upstream_timeout_secs: 600 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    /// Days of request log to keep.
    pub retention_days: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { path: PathBuf::from("xolotl.db"), retention_days: 90 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VaultConfig {
    pub address: String,
    /// KV v2 mount.
    pub mount: String,
    /// Path prefix inside the mount under which all xolotl secrets live.
    pub prefix: String,
    pub namespace: Option<String>,
    /// PEM CA bundle for a privately signed OpenBao certificate.
    pub ca_cert: Option<PathBuf>,
    pub approle: AppRoleConfig,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            address: "http://127.0.0.1:8200".into(),
            mount: "secret".into(),
            prefix: "xolotl".into(),
            namespace: None,
            ca_cert: None,
            approle: AppRoleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppRoleConfig {
    pub mount: String,
    pub role_id: Option<String>,
    pub role_id_file: Option<PathBuf>,
    pub secret_id: Option<String>,
    pub secret_id_file: Option<PathBuf>,
}

impl Default for AppRoleConfig {
    fn default() -> Self {
        Self { mount: "approle".into(), role_id: None, role_id_file: None, secret_id: None, secret_id_file: None }
    }
}

impl AppRoleConfig {
    pub fn resolve_role_id(&self) -> Result<String> {
        resolve_inline_or_file(&self.role_id, &self.role_id_file, "vault.approle.role_id")
    }
    pub fn resolve_secret_id(&self) -> Result<String> {
        resolve_inline_or_file(&self.secret_id, &self.secret_id_file, "vault.approle.secret_id")
    }
}

fn resolve_inline_or_file(inline: &Option<String>, file: &Option<PathBuf>, name: &str) -> Result<String> {
    if let Some(v) = inline.as_deref().filter(|v| !v.is_empty()) {
        return Ok(v.to_string());
    }
    if let Some(path) = file {
        let v = std::fs::read_to_string(path).with_context(|| format!("reading {name} from {}", path.display()))?;
        return Ok(v.trim().to_string());
    }
    anyhow::bail!("{name} is not configured (set it inline, via *_file, or the environment)")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// OTLP/HTTP base endpoint, e.g. `http://openobserve:5080/api/default`.
    /// `/v1/traces`, `/v1/metrics` and `/v1/logs` are appended. Empty disables export.
    pub otlp_endpoint: Option<String>,
    /// Extra OTLP headers (e.g. `Authorization`). Values may also be loaded from
    /// the vault secret `telemetry` (keys are header names), which wins.
    pub otlp_headers: BTreeMap<String, String>,
    pub service_name: String,
    /// `pretty` or `json` for stdout logs.
    pub log_format: String,
    /// Default `RUST_LOG`-style filter.
    pub log_filter: String,
    pub metrics_interval_secs: u64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            otlp_headers: BTreeMap::new(),
            service_name: "xolotl".into(),
            log_format: "pretty".into(),
            log_filter: "info".into(),
            metrics_interval_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Spread requests evenly across healthy accounts.
    #[default]
    RoundRobin,
    /// Use the first healthy account (by priority) until it is limited.
    FillFirst,
    /// Prefer the account with the most remaining quota in its shortest window.
    LeastUsed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubscriptionConfig {
    pub strategy: Strategy,
    /// How often to poll each account's usage endpoint. 0 disables polling.
    pub usage_poll_secs: u64,
    /// How many different accounts to try for a single request.
    pub max_attempts: usize,
    /// Cooldown applied after a 429 whose reset time is unknown.
    pub default_cooldown_secs: u64,
    /// Refresh OAuth tokens this many seconds before they expire.
    pub refresh_margin_secs: i64,
    /// Accounts above this usage percent in any window are only used as a last resort.
    pub soft_limit_percent: f64,
    /// Keep a session (Codex session id, Claude Code session id) on one account
    /// for this long, preserving prompt caches. 0 disables affinity.
    pub session_affinity_secs: u64,
    /// Upstream URLs; only override for testing or if a provider moves.
    pub endpoints: Endpoints,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Endpoints {
    pub codex_backend: String,
    pub codex_auth: String,
    pub claude_api: String,
    pub claude_authorize: String,
    pub claude_token: String,
    pub claude_manual_redirect: String,
    pub opencode_go: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            codex_backend: "https://chatgpt.com/backend-api".into(),
            codex_auth: "https://auth.openai.com".into(),
            claude_api: "https://api.anthropic.com".into(),
            claude_authorize: "https://claude.ai/oauth/authorize".into(),
            claude_token: "https://platform.claude.com/v1/oauth/token".into(),
            claude_manual_redirect: "https://platform.claude.com/oauth/code/callback".into(),
            opencode_go: "https://opencode.ai/zen/go".into(),
        }
    }
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::RoundRobin,
            usage_poll_secs: 300,
            max_attempts: 3,
            default_cooldown_secs: 60,
            refresh_margin_secs: 300,
            soft_limit_percent: 95.0,
            session_affinity_secs: 3600,
            endpoints: Endpoints::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    /// Providers keyed by the name used as a model prefix (`<name>/<model>`)
    /// and as a passthrough path prefix (`/<name>/...`).
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        for (name, kind) in [("vercel", ProviderKind::Vercel), ("mistral", ProviderKind::Mistral)] {
            providers.insert(name.to_string(), ProviderConfig { kind, ..ProviderConfig::default() });
        }
        Self { providers }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Vercel,
    Mistral,
    /// Any OpenAI-compatible server, e.g. llama.cpp.
    #[default]
    OpenaiCompatible,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub enabled: Option<bool>,
    /// Override the provider's default base URL (required for openai-compatible).
    pub base_url: Option<String>,
    /// Vault secret (relative to `vault.prefix`) that holds `api_key`.
    /// Defaults to `providers/<name>`. Set `no_auth = true` for keyless servers.
    pub secret: Option<String>,
    pub no_auth: bool,
    /// Extra static headers sent upstream.
    pub headers: BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) => {
                let raw = std::fs::read_to_string(p).with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&raw).with_context(|| format!("parsing config {}", p.display()))?
            }
            None => Config::default(),
        };
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    fn apply_env(&mut self) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("XOLOTL_LISTEN").and_then(|v| v.parse().ok()) {
            self.server.listen = v;
        }
        if let Some(v) = env("XOLOTL_DATABASE") {
            self.database.path = v.into();
        }
        if let Some(v) = env("BAO_ADDR").or_else(|| env("VAULT_ADDR")) {
            self.vault.address = v;
        }
        if let Some(v) = env("BAO_NAMESPACE").or_else(|| env("VAULT_NAMESPACE")) {
            self.vault.namespace = Some(v);
        }
        if let Some(v) = env("BAO_CACERT").or_else(|| env("VAULT_CACERT")) {
            self.vault.ca_cert = Some(v.into());
        }
        if let Some(v) = env("XOLOTL_VAULT_ROLE_ID") {
            self.vault.approle.role_id = Some(v);
        }
        if let Some(v) = env("XOLOTL_VAULT_SECRET_ID") {
            self.vault.approle.secret_id = Some(v);
        }
        if let Some(v) = env("OTEL_EXPORTER_OTLP_ENDPOINT") {
            self.telemetry.otlp_endpoint = Some(v);
        }
        if let Some(v) = env("XOLOTL_LOG_FORMAT") {
            self.telemetry.log_format = v;
        }
    }

    pub fn validate(&self) -> Result<()> {
        for (name, p) in &self.gateway.providers {
            anyhow::ensure!(
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "provider name {name:?} must be alphanumeric, '-' or '_'"
            );
            anyhow::ensure!(
                !crate::subscription::RESERVED_PROVIDER_NAMES.contains(&name.as_str()),
                "provider name {name:?} is reserved for subscription accounts"
            );
            anyhow::ensure!(
                !["v0", "v1", "admin", "ui", "backend-api", "healthz", "readyz"].contains(&name.as_str()),
                "provider name {name:?} collides with a built-in route"
            );
            if p.kind == ProviderKind::OpenaiCompatible {
                anyhow::ensure!(p.base_url.is_some(), "provider {name:?}: openai-compatible providers need base_url");
            }
        }
        anyhow::ensure!(self.subscription.max_attempts >= 1, "subscription.max_attempts must be >= 1");
        anyhow::ensure!(matches!(self.telemetry.log_format.as_str(), "pretty" | "json"), "telemetry.log_format must be pretty or json");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let raw = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.gateway.providers.contains_key("llamacpp"));
    }

    #[test]
    fn rejects_openai_compatible_without_base_url() {
        let cfg: Config = toml::from_str("[gateway.providers.local]\nkind = \"openai-compatible\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}
