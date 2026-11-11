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
    pub mcp: McpConfig,
    pub ui: UiConfig,
    pub sessions: SessionsConfig,
    pub export: ExportConfig,
}

/// Copies of LLM exchanges sent elsewhere for analysis.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExportConfig {
    /// Each session request and its response as a row in a ClickHouse table.
    pub clickhouse: Option<ClickHouseConfig>,
    /// The same rows, published to a NATS JetStream stream that ClickHouse loads.
    pub nats: Option<NatsConfig>,
}

/// A NATS JetStream stream. Each row is published to `xolotl.exchanges.<client_id>`.
/// Credentials are the vault secret `nats`: `user` and `password`, or `creds`
/// (the contents of a `.creds` file).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NatsConfig {
    /// Server URL, e.g. `nats://nats:4222`, or `tls://` to require TLS.
    /// Several servers of one cluster are separated by commas.
    pub url: String,
    /// The stream that stores the rows. It must exist, and its subjects must
    /// cover `xolotl.exchanges.>`; xolotl checks both at startup.
    pub stream: String,
    /// PEM CA bundle for a privately signed NATS certificate.
    pub ca_cert: Option<PathBuf>,
    /// Include the request and response bodies. Without them a row holds the
    /// request log fields only.
    pub bodies: bool,
    /// Longer request and response bodies are cut, and the row says so. A row
    /// larger than the server's `max_payload` is cut further.
    pub max_body_bytes: usize,
    /// Megabytes of rows waiting to be published. Rows wait while NATS is
    /// unreachable; rows past it are dropped and counted.
    pub queue_mb: usize,
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self { url: String::new(), stream: "XOLOTL".into(), ca_cert: None, bodies: true, max_body_bytes: 4 * 1024 * 1024, queue_mb: 32 }
    }
}

/// A ClickHouse table, written through the HTTP interface. The password is
/// the vault secret `clickhouse` (fields `password` and, optionally, `user`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClickHouseConfig {
    /// HTTP interface, e.g. `http://clickhouse:8123`.
    pub url: String,
    pub database: String,
    pub table: String,
    /// ClickHouse user, unless the vault secret names one.
    pub user: Option<String>,
    /// PEM CA bundle for a privately signed ClickHouse certificate.
    pub ca_cert: Option<PathBuf>,
    /// Create the table before the first insert when it doesn't exist. The database must exist.
    pub create_table: bool,
    /// Rows older than this are deleted by ClickHouse. Applies to a table xolotl creates. 0 keeps them.
    pub ttl_days: u32,
    /// Include the request and response bodies. Without them a row holds the
    /// request log fields only.
    pub bodies: bool,
    /// Longer request and response bodies are cut, and the row says so.
    pub max_body_bytes: usize,
    /// Rows per insert.
    pub batch_rows: usize,
    /// Longest a row waits before it is inserted.
    pub flush_secs: u64,
    /// Megabytes of rows waiting to be inserted; rows past it are dropped and counted.
    pub queue_mb: usize,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            database: "xolotl".into(),
            table: "llm_exchanges".into(),
            user: None,
            ca_cert: None,
            create_table: true,
            ttl_days: 0,
            bodies: true,
            max_body_bytes: 4 * 1024 * 1024,
            batch_rows: 100,
            flush_secs: 5,
            queue_mb: 256,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    /// Pin a session to its account and model once its prompt reaches this many
    /// tokens, so that failover and routing keep the prompt cache. 0 disables it.
    pub lock_tokens: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self { lock_tokens: 60_000 }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Web UI sign-in. Without it the UI can't be signed in to; the CLI and
    /// API keys still work.
    pub oidc: Option<OidcConfig>,
}

/// An OpenID Connect provider (e.g. Pocket ID). The client secret is the
/// vault secret `oidc` (field `client_secret`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// `https://<xolotl>/admin/session/callback`, as registered with the provider.
    pub redirect_url: String,
    /// Only members of this group (the `groups` claim) are signed in.
    pub admin_group: String,
}

/// The MCP gateway: upstream MCP servers, exposed to clients through one
/// code-mode MCP endpoint at `/mcp`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Upstream servers keyed by name. Each entry is one instance: the same
    /// server URL can be listed twice under different names to use two sets
    /// of credentials. Secrets come from the vault at `mcp/<name>`.
    pub servers: BTreeMap<String, McpServerConfig>,
    /// Wall-clock limit for one `execute` call, tool calls included.
    pub execute_timeout_secs: u64,
    /// QuickJS heap limit for one `execute` call.
    pub memory_limit_mb: usize,
    /// Upstream tool calls allowed in one `execute` call.
    pub max_tool_calls: usize,
    /// Larger `execute` results and logs are truncated.
    pub max_output_bytes: usize,
    /// `execute` calls that may run at once; more wait.
    pub max_concurrent_executions: usize,
    /// How often upstream tool lists are re-read.
    pub tools_refresh_secs: u64,
    /// Browser origins allowed to call `/mcp`. Empty rejects every request
    /// that carries an `Origin` header (MCP clients other than browsers send none).
    pub allowed_origins: Vec<String>,
    /// OAuth logins to upstream servers (`auth = "oauth"`).
    pub oauth: McpOAuthConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpOAuthConfig {
    /// Where the authorization server sends the browser after consent.
    /// Default: xolotl's callback under `server.public_url` when that is set,
    /// otherwise a loopback URL whose (unloadable) page URL is pasted back.
    pub redirect_uri: Option<String>,
    /// HTTPS URL of xolotl's OAuth Client ID Metadata Document, used as the
    /// client id with authorization servers that support it. Defaults to the
    /// document xolotl serves under `server.public_url` when that is HTTPS.
    pub client_metadata_url: Option<String>,
    /// Client name shown on consent screens.
    pub client_name: String,
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self { redirect_uri: None, client_metadata_url: None, client_name: "xolotl".into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuth {
    /// Static headers (with `{field}` placeholders from the vault).
    #[default]
    Headers,
    /// An OAuth login (`xolotl mcp login <server>`); headers are sent as well.
    Oauth,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerOAuthConfig {
    /// A client registered with the authorization server beforehand. Its
    /// secret, if any, goes in the vault secret `mcp/<name>` as `client_secret`.
    /// Without one, xolotl uses a Client ID Metadata Document or Dynamic
    /// Client Registration.
    pub client_id: Option<String>,
    /// Scopes to request. Default: what the server asks for.
    pub scopes: Vec<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            execute_timeout_secs: 120,
            memory_limit_mb: 64,
            max_tool_calls: 100,
            max_output_bytes: 100_000,
            max_concurrent_executions: 8,
            tools_refresh_secs: 300,
            allowed_origins: Vec::new(),
            oauth: McpOAuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Streamable HTTP endpoint of the upstream server.
    pub url: String,
    /// Shown to clients by `search` and in the admin UI.
    pub description: String,
    /// Headers sent upstream. `{field}` is replaced by that field of the
    /// vault secret `mcp/<name>`, e.g. `Authorization = "Bearer {token}"`.
    pub headers: BTreeMap<String, String>,
    /// Limit for one upstream tool call. Default 60.
    pub timeout_secs: Option<u64>,
    pub auth: McpAuth,
    pub oauth: McpServerOAuthConfig,
}

impl McpServerConfig {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(60).max(1))
    }

    /// Checks shared by servers from the config file and from the admin API.
    pub fn validate(&self, name: &str) -> Result<()> {
        anyhow::ensure!(
            valid_mcp_server_name(name),
            "mcp server name {name:?} must start with a lowercase letter and use lowercase letters, digits, '-' or '_' (max 63)"
        );
        anyhow::ensure!(
            !crate::mcp::codemode::reserved_identifier(&name.replace('-', "_")),
            "mcp server name {name:?} is a JavaScript keyword or a global in code mode; choose another name"
        );
        let url = url::Url::parse(&self.url).with_context(|| format!("mcp server {name:?}: invalid url"))?;
        anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "mcp server {name:?}: url must be http or https");
        for (header, value) in &self.headers {
            anyhow::ensure!(
                axum::http::HeaderName::from_bytes(header.as_bytes()).is_ok(),
                "mcp server {name:?}: invalid header name {header:?}"
            );
            crate::mcp::template_fields(value).with_context(|| format!("mcp server {name:?}: header {header:?}"))?;
        }
        anyhow::ensure!(
            self.oauth.client_id.is_none() && self.oauth.scopes.is_empty() || self.auth == McpAuth::Oauth,
            "mcp server {name:?}: oauth settings need auth = \"oauth\""
        );
        Ok(())
    }
}

/// Server names that become the same JavaScript identifier (`-` turns into `_`).
pub fn colliding_mcp_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<(&'a str, &'a str)> {
    let mut idents = BTreeMap::new();
    for name in names {
        if let Some(other) = idents.insert(name.replace('-', "_"), name) {
            return Some((other, name));
        }
    }
    None
}

/// MCP server names become JavaScript identifiers in code mode (`-` turns
/// into `_`), so they are lowercase, start with a letter and stay short.
pub fn valid_mcp_server_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogConfig {
    /// LiteLLM registry source. Empty disables network refresh.
    pub metadata_url: String,
    pub metadata: BTreeMap<String, crate::metadata::Override>,
    /// How often to re-discover provider model lists.
    pub refresh_secs: u64,
    /// Codex filters its model roster by client version.
    pub codex_client_version: String,
    /// Grok CLI version sent to xAI's chat proxy, which rejects old clients
    /// with 426. Requests from the Grok CLI send its own version instead.
    pub grok_client_version: String,
    /// Extra models per provider, merged with discovered ones.
    pub static_models: BTreeMap<String, Vec<String>>,
    /// Wire-format overrides keyed by `provider` or `provider/model`.
    pub wires: BTreeMap<String, Vec<crate::catalog::Wire>>,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 600,
            metadata_url: crate::metadata::REGISTRY_URL.into(),
            metadata: BTreeMap::new(),
            codex_client_version: "0.160.0".into(),
            grok_client_version: "1.0.52".into(),
            static_models: BTreeMap::new(),
            wires: BTreeMap::new(),
        }
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
    /// The URL browsers reach xolotl at, e.g. `https://xolotl.example.lan`.
    /// When set, OAuth logins to MCP servers redirect back to xolotl, which
    /// finishes them without pasting a URL.
    pub public_url: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8317".parse().unwrap(), max_body_bytes: 64 * 1024 * 1024, upstream_timeout_secs: 600, public_url: None }
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
    /// Where operators look at traces and logs (e.g. OpenObserve); the web UI links to it.
    pub dashboard_url: Option<String>,
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
            dashboard_url: None,
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
    /// Keep a session (Codex session id, Claude Code session id, Grok session id) on one account
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
    /// xAI's CLI chat proxy (Grok Build subscriptions).
    pub grok_api: String,
    /// xAI's OAuth issuer; endpoints come from its OIDC discovery document.
    pub grok_auth: String,
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
            grok_api: "https://cli-chat-proxy.grok.com/v1".into(),
            grok_auth: "https://auth.x.ai".into(),
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

    /// Check a provider and the name it is addressed by.
    pub fn validate(&self, name: &str) -> Result<()> {
        validate_provider_name(name)?;
        if self.kind == ProviderKind::OpenaiCompatible {
            anyhow::ensure!(self.base_url.is_some(), "provider {name:?}: openai-compatible providers need base_url");
        }
        Ok(())
    }

    /// [`Self::validate`], plus the checks for a provider account added at
    /// runtime: the config file keeps its looser rules so existing files load.
    pub fn validate_account(&self, name: &str) -> Result<()> {
        self.validate(name)?;
        anyhow::ensure!(self.secret.is_none(), "provider {name:?}: a provider account keeps its key with it; secret can't be set");
        if let Some(url) = &self.base_url {
            anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "provider {name:?}: base_url must start with http:// or https://"
            );
        }
        for (k, v) in &self.headers {
            anyhow::ensure!(k.parse::<axum::http::HeaderName>().is_ok(), "provider {name:?}: {k:?} is not a header name");
            anyhow::ensure!(v.parse::<axum::http::HeaderValue>().is_ok(), "provider {name:?}: header {k:?} has an invalid value");
        }
        Ok(())
    }
}

/// Provider names are model prefixes and path prefixes, so they can't take
/// the subscription pools' names or a built-in route's.
pub fn validate_provider_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "provider name {name:?} must be alphanumeric, '-' or '_'"
    );
    anyhow::ensure!(
        !crate::subscription::RESERVED_PROVIDER_NAMES.contains(&name),
        "provider name {name:?} is reserved for subscription accounts"
    );
    anyhow::ensure!(
        !["v0", "v1", "admin", "ui", "assets", "backend-api", "healthz", "readyz", "mcp"].contains(&name),
        "provider name {name:?} collides with a built-in route"
    );
    Ok(())
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

    /// A copy that is safe to print: credentials and header values replaced.
    pub fn redacted(&self) -> Config {
        const HIDDEN: &str = "<redacted>";
        let mut c = self.clone();
        for v in [&mut c.vault.approle.role_id, &mut c.vault.approle.secret_id].into_iter().flatten() {
            *v = HIDDEN.into();
        }
        let headers = c
            .telemetry
            .otlp_headers
            .values_mut()
            .chain(c.gateway.providers.values_mut().flat_map(|p| p.headers.values_mut()))
            .chain(c.mcp.servers.values_mut().flat_map(|s| s.headers.values_mut()));
        for v in headers {
            *v = HIDDEN.into();
        }
        c
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
        if let Some(v) = env("XOLOTL_CLICKHOUSE_URL") {
            self.export.clickhouse.get_or_insert_with(Default::default).url = v;
        }
        if let Some(v) = env("XOLOTL_NATS_URL") {
            self.export.nats.get_or_insert_with(Default::default).url = v;
        }
    }

    pub fn validate(&self) -> Result<()> {
        for (name, p) in &self.gateway.providers {
            p.validate(name)?;
        }
        for (key, entry) in &self.catalog.metadata {
            anyhow::ensure!(
                key.split_once('/').is_some_and(|(p, m)| !p.is_empty() && !m.is_empty()),
                "catalog.metadata key must be provider/model"
            );
            entry.values.validate()?;
            if let Some(levels) = &entry.reasoning_levels {
                anyhow::ensure!(
                    levels.iter().all(|l| ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&l.as_str())),
                    "invalid catalog reasoning level"
                );
            }
        }
        for key in self.catalog.wires.keys() {
            anyhow::ensure!(
                key != "claude" && !key.starts_with("claude/"),
                "catalog.wires.{key:?}: Claude subscription accounts serve only /v1/messages; their wire formats can't be overridden"
            );
        }
        if let Some(o) = &self.ui.oidc {
            url::Url::parse(&o.issuer).context("ui.oidc.issuer must be a URL")?;
            let redirect = url::Url::parse(&o.redirect_url).context("ui.oidc.redirect_url must be a URL")?;
            anyhow::ensure!(
                redirect.path() == crate::server::session::CALLBACK_PATH,
                "ui.oidc.redirect_url must end in {}",
                crate::server::session::CALLBACK_PATH
            );
            anyhow::ensure!(!o.client_id.is_empty() && !o.admin_group.is_empty(), "ui.oidc needs client_id and admin_group");
        }
        for (name, server) in &self.mcp.servers {
            server.validate(name)?;
        }
        if let Some((a, b)) = colliding_mcp_names(self.mcp.servers.keys().map(String::as_str)) {
            anyhow::bail!("mcp servers {a:?} and {b:?} have the same name in code ('-' becomes '_')");
        }
        anyhow::ensure!(self.mcp.max_concurrent_executions >= 1, "mcp.max_concurrent_executions must be >= 1");
        if let Some(u) = &self.server.public_url {
            let url = url::Url::parse(u).context("server.public_url: invalid url")?;
            anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "server.public_url must be http or https");
        }
        if let Some(u) = &self.mcp.oauth.redirect_uri {
            url::Url::parse(u).context("mcp.oauth.redirect_uri: invalid url")?;
        }
        if let Some(u) = &self.mcp.oauth.client_metadata_url {
            let url = url::Url::parse(u).context("mcp.oauth.client_metadata_url: invalid url")?;
            anyhow::ensure!(url.scheme() == "https" && url.path() != "/", "mcp.oauth.client_metadata_url must be an https URL with a path");
        }
        if let Some(ch) = &self.export.clickhouse {
            let url = url::Url::parse(&ch.url).context("export.clickhouse.url: invalid url")?;
            anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "export.clickhouse.url must be http or https");
            // A query string could carry `user` and `password`, and the url is logged.
            anyhow::ensure!(
                url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
                "export.clickhouse.url must not hold credentials or a query string; put credentials in the vault secret `clickhouse`"
            );
            let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            anyhow::ensure!(ident(&ch.database) && ident(&ch.table), "export.clickhouse.database and table must be alphanumeric or '_'");
            anyhow::ensure!(ch.batch_rows >= 1, "export.clickhouse.batch_rows must be >= 1");
            anyhow::ensure!(ch.queue_mb >= 1, "export.clickhouse.queue_mb must be >= 1");
        }
        if let Some(n) = &self.export.nats {
            anyhow::ensure!(!n.url.trim().is_empty(), "export.nats.url must be set");
            for server in n.url.split(',') {
                let url = url::Url::parse(server.trim()).context("export.nats.url: invalid url")?;
                anyhow::ensure!(matches!(url.scheme(), "nats" | "tls"), "export.nats.url must be nats:// or tls://");
                // The url is logged.
                anyhow::ensure!(
                    url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
                    "export.nats.url must not hold credentials or a query string; put credentials in the vault secret `nats`"
                );
            }
            anyhow::ensure!(
                !n.stream.is_empty() && n.stream.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "export.nats.stream must be alphanumeric, '-' or '_'"
            );
            anyhow::ensure!(n.queue_mb >= 1, "export.nats.queue_mb must be >= 1");
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
    fn redacted_hides_credentials() {
        let mut cfg: Config = toml::from_str(
            "[vault.approle]\nrole_id = \"role\"\nsecret_id = \"s3cret\"\n[telemetry.otlp_headers]\nAuthorization = \"Basic abc\"\n\
             [gateway.providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://x\"\nheaders = { X-Key = \"hdr\" }\n",
        )
        .unwrap();
        cfg.vault.approle.secret_id_file = Some("/run/secret_id".into());
        let out = toml::to_string_pretty(&cfg.redacted()).unwrap();
        for secret in ["role", "s3cret", "Basic abc", "hdr"] {
            assert!(!out.contains(&format!("\"{secret}\"")), "{secret} leaked:\n{out}");
        }
        assert!(out.contains("/run/secret_id"), "file paths are not secret");
    }

    #[test]
    fn clickhouse_export_is_validated() {
        let cfg: Config = toml::from_str("[export.clickhouse]\nurl = \"http://ch:8123\"\n").unwrap();
        cfg.validate().unwrap();
        let ch = cfg.export.clickhouse.unwrap();
        assert_eq!((ch.database.as_str(), ch.table.as_str(), ch.bodies), ("xolotl", "llm_exchanges", true));
        for bad in [
            "url = \"ch:8123\"",
            "url = \"http://ch\"\ntable = \"x; DROP TABLE y\"",
            "url = \"http://ch\"\nbatch_rows = 0",
            "url = \"http://u:p@ch:8123\"",
            "url = \"https://ch:8443/?user=xolotl&password=secret\"",
            "url = \"http://ch:8123/?\"",
        ] {
            let cfg: Config = toml::from_str(&format!("[export.clickhouse]\n{bad}\n")).unwrap();
            assert!(cfg.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn nats_export_is_validated() {
        let cfg: Config = toml::from_str("[export.nats]\nurl = \"nats://n1:4222, nats://n2:4222\"\n").unwrap();
        cfg.validate().unwrap();
        let n = cfg.export.nats.unwrap();
        assert_eq!((n.stream.as_str(), n.bodies, n.queue_mb), ("XOLOTL", true, 32));
        for bad in [
            "url = \"\"",
            "url = \"http://n:4222\"",
            "url = \"nats://u:p@n:4222\"",
            "url = \"nats://n:4222\"\nstream = \"a.b\"",
            "url = \"nats://n:4222\"\nqueue_mb = 0",
        ] {
            let cfg: Config = toml::from_str(&format!("[export.nats]\n{bad}\n")).unwrap();
            assert!(cfg.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_wire_overrides_for_claude_accounts() {
        for key in ["claude", "\"claude/claude-opus-5\""] {
            let cfg: Config = toml::from_str(&format!("[catalog.wires]\n{key} = [\"chat\"]\n")).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("/v1/messages"), "{err}");
        }
        let cfg: Config = toml::from_str("[catalog.wires]\nllamacpp = [\"chat\", \"messages\"]\n").unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn oidc_redirect_must_be_the_callback() {
        let oidc = |redirect: &str| {
            format!(
                "[ui.oidc]\nissuer = \"https://id.example.test\"\nclient_id = \"c\"\nredirect_url = \"{redirect}\"\nadmin_group = \"admins\"\n"
            )
        };
        let cfg: Config = toml::from_str(&oidc("https://gw.example.test/admin/session/callback")).unwrap();
        cfg.validate().unwrap();
        let cfg: Config = toml::from_str(&oidc("https://gw.example.test/callback")).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_openai_compatible_without_base_url() {
        let cfg: Config = toml::from_str("[gateway.providers.local]\nkind = \"openai-compatible\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }
}
