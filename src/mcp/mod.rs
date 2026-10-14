//! MCP gateway. Upstream MCP servers from `[mcp.servers]` are exposed to
//! clients through one code-mode MCP server at `/mcp`: instead of every
//! upstream tool, a client sees `search` (TypeScript declarations of the
//! tools its policy allows) and `execute` (JavaScript that calls them).

pub mod codemode;
pub mod oauth;
pub mod server;
mod upstream;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use opentelemetry::KeyValue;
use parking_lot::RwLock;
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::{Config, McpAuth, McpConfig, McpServerConfig, colliding_mcp_names};
use crate::db::{Db, McpCallRecord};
use crate::keys::Principal;
use crate::policy::Policy;
use crate::secrets::Secrets;
use crate::telemetry::metrics;
use upstream::Upstream;
pub use upstream::{CallError, UpstreamStatus};

/// The `{field}` placeholders of a header template. Braces are not allowed otherwise.
pub fn template_fields(template: &str) -> Result<Vec<String>> {
    let mut fields = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find(['{', '}']) {
        anyhow::ensure!(rest.as_bytes()[start] == b'{', "unmatched '}}'");
        let after = &rest[start + 1..];
        let end = after.find('}').ok_or_else(|| anyhow::anyhow!("unclosed '{{'"))?;
        let field = &after[..end];
        anyhow::ensure!(
            !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "invalid placeholder {{{field}}}: use letters, digits, '_' or '-'"
        );
        fields.push(field.to_string());
        rest = &after[end + 1..];
    }
    Ok(fields)
}

/// Fill a header template's placeholders from a vault secret.
pub fn render_template(template: &str, secret: &Map<String, Value>) -> Result<String> {
    let mut out = template.to_string();
    for field in template_fields(template)? {
        let value = secret
            .get(&field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("secret field {field:?} is not set"))?;
        out = out.replace(&format!("{{{field}}}"), value);
    }
    Ok(out)
}

/// A tool a principal may call.
#[derive(Debug, Clone)]
pub struct AllowedTool {
    pub server: String,
    pub tool: Tool,
}

/// Where a server's definition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerSource {
    /// `[mcp.servers]` in the config file; read-only at runtime.
    Config,
    /// Added through the admin API; stored in the vault under [`SERVERS_PREFIX`].
    Vault,
}

/// Vault directory of servers added at runtime: `mcp-servers/<name>`.
pub const SERVERS_PREFIX: &str = "mcp-servers";

/// An upstream server as the admin API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub url: String,
    pub description: String,
    pub source: ServerSource,
    /// The stored definition, for editing; only for servers from the vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<McpServerConfig>,
    /// Name used in code mode (`-` becomes `_`).
    pub namespace: String,
    /// Vault secret holding the header placeholder values.
    pub secret: String,
    /// Placeholder fields the headers use, and whether each is set.
    pub secret_fields: BTreeMap<String, bool>,
    #[serde(flatten)]
    pub status: UpstreamStatus,
}

/// Why adding, changing or removing a server was refused.
#[derive(Debug, thiserror::Error)]
pub enum ServerChangeError {
    #[error("no MCP server named {0}")]
    NotFound(String),
    /// The name is taken, comes from the config file, is still in use, or changed concurrently.
    #[error("{0}")]
    Conflict(String),
    #[error("{0:#}")]
    Invalid(anyhow::Error),
    #[error("{0:#}")]
    Store(anyhow::Error),
}

#[derive(Clone)]
struct Entry {
    up: Arc<Upstream>,
    source: ServerSource,
}

pub struct McpHub {
    pub cfg: McpConfig,
    /// The whole config, for the OAuth redirect URI (`server.public_url`).
    app: Config,
    secrets: Secrets,
    pub flows: oauth::Flows,
    db: Db,
    servers: RwLock<BTreeMap<String, Entry>>,
    /// Serializes reloads and server changes.
    changes: tokio::sync::Mutex<()>,
    executions: Arc<tokio::sync::Semaphore>,
}

/// Whether a changed definition still uses the same OAuth login.
fn same_login(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.auth == McpAuth::Oauth && b.auth == McpAuth::Oauth && a.url == b.url && a.oauth == b.oauth
}

/// The JavaScript identifier of a server name.
pub fn namespace(server: &str) -> String {
    server.replace('-', "_")
}

impl McpHub {
    /// A hub with the config file's servers. [`McpHub::reload`] adds the vault's.
    pub fn new(app: &Config, secrets: Secrets, db: Db) -> Self {
        let cfg = &app.mcp;
        let servers = cfg
            .servers
            .iter()
            .map(|(name, s)| {
                let up = Arc::new(Upstream::new(name.clone(), s.clone(), secrets.clone()));
                (name.clone(), Entry { up, source: ServerSource::Config })
            })
            .collect();
        Self {
            cfg: cfg.clone(),
            app: app.clone(),
            secrets,
            flows: Default::default(),
            db,
            servers: RwLock::new(servers),
            changes: Default::default(),
            executions: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent_executions.max(1))),
        }
    }

    fn entries(&self) -> Vec<(String, Entry)> {
        self.servers.read().iter().map(|(n, e)| (n.clone(), e.clone())).collect()
    }

    fn upstreams(&self) -> Vec<Arc<Upstream>> {
        self.servers.read().values().map(|e| e.up.clone()).collect()
    }

    fn get(&self, name: &str) -> Option<Arc<Upstream>> {
        self.servers.read().get(name).map(|e| e.up.clone())
    }

    fn server(&self, name: &str) -> Result<Arc<Upstream>> {
        self.get(name).ok_or_else(|| anyhow::anyhow!("no such MCP server"))
    }

    // ---- servers stored in the vault

    /// Read the servers stored in the vault and apply them. Unchanged servers
    /// keep their connection, tools and login; changed ones start over.
    /// Invalid entries, and names the config file already uses, are skipped.
    pub async fn reload(&self) -> Result<usize> {
        let _changes = self.changes.lock().await;
        self.reload_locked().await
    }

    async fn reload_locked(&self) -> Result<usize> {
        let mut stored = BTreeMap::new();
        for key in self.secrets.list(SERVERS_PREFIX).await? {
            if key.ends_with('/') {
                continue;
            }
            let Some(data) = self.secrets.get(&format!("{SERVERS_PREFIX}/{key}")).await? else { continue };
            let parsed = serde_json::from_value::<McpServerConfig>(Value::Object(data)).map_err(anyhow::Error::from);
            match parsed.and_then(|cfg| cfg.validate(&key).map(|()| cfg)) {
                Ok(_) if self.cfg.servers.contains_key(&key) => {
                    tracing::warn!(server = %key, "MCP server in the vault ignored: the config file defines one with that name");
                }
                Ok(cfg) => {
                    stored.insert(key, cfg);
                }
                Err(e) => tracing::warn!(server = %key, error = %format!("{e:#}"), "invalid MCP server in the vault ignored"),
            }
        }
        let current = self.entries();
        let mut next: BTreeMap<String, Entry> = current.iter().filter(|(_, e)| e.source == ServerSource::Config).cloned().collect();
        for (name, cfg) in stored {
            if let Some((a, b)) = colliding_mcp_names(next.keys().map(String::as_str).chain([name.as_str()])) {
                tracing::warn!(server = %name, "MCP server in the vault ignored: {a:?} and {b:?} have the same name in code");
                continue;
            }
            let previous = current.iter().find(|(n, e)| *n == name && e.source == ServerSource::Vault).map(|(_, e)| &e.up);
            let up = match previous {
                Some(up) if up.cfg == cfg => up.clone(),
                // Same login when nothing it depends on changed (e.g. the description).
                Some(up) if same_login(&up.cfg, &cfg) => {
                    Arc::new(Upstream::with_store(name.clone(), cfg, self.secrets.clone(), up.oauth.clone()))
                }
                _ => Arc::new(Upstream::new(name.clone(), cfg, self.secrets.clone())),
            };
            next.insert(name, Entry { up, source: ServerSource::Vault });
        }
        let gone: Vec<Arc<Upstream>> =
            current.iter().filter(|(n, e)| next.get(n).is_none_or(|x| !Arc::ptr_eq(&x.up, &e.up))).map(|(_, e)| e.up.clone()).collect();
        let live: Vec<Arc<oauth::OAuthStore>> = next.values().filter_map(|e| e.up.oauth.clone()).collect();
        let n = next.values().filter(|e| e.source == ServerSource::Vault).count();
        *self.servers.write() = next;
        for up in gone {
            up.disconnect().await;
            // A login no server uses any more: stop it, and its logins in progress.
            if let Some(store) = &up.oauth
                && !live.iter().any(|s| Arc::ptr_eq(s, store))
            {
                store.retire().await;
                self.flows.cancel_server(&up.name);
            }
        }
        // Pick up sign-outs, logins and refreshes made by other instances.
        for store in live {
            store.sync().await;
        }
        Ok(n)
    }

    /// Add (`create`) or change a server stored in the vault.
    pub async fn put_server(&self, name: &str, cfg: McpServerConfig, create: bool) -> Result<(), ServerChangeError> {
        cfg.validate(name).map_err(ServerChangeError::Invalid)?;
        let _changes = self.changes.lock().await;
        if self.cfg.servers.contains_key(name) {
            return Err(ServerChangeError::Conflict(format!("MCP server {name} is defined in the config file; change it there")));
        }
        let names = self.server_names();
        if let Some((a, b)) = colliding_mcp_names(names.iter().map(String::as_str).filter(|n| *n != name).chain([name])) {
            return Err(ServerChangeError::Conflict(format!("{a:?} and {b:?} would have the same name in code ('-' becomes '_')")));
        }
        let key = format!("{SERVERS_PREFIX}/{name}");
        let (existing, version) = self.secrets.get_versioned(&key).await.map_err(ServerChangeError::Store)?;
        match (create, existing.is_some()) {
            (true, true) => return Err(ServerChangeError::Conflict(format!("an MCP server named {name} already exists"))),
            (false, false) => return Err(ServerChangeError::NotFound(name.to_string())),
            _ => {}
        }
        let data = match serde_json::to_value(&cfg).map_err(|e| ServerChangeError::Store(e.into()))? {
            Value::Object(m) => m,
            _ => unreachable!("server definitions serialize to objects"),
        };
        self.secrets.put_cas(&key, data, version).await.map_err(|e| match e.downcast_ref::<crate::secrets::Conflict>() {
            Some(_) => ServerChangeError::Conflict(format!("MCP server {name} was changed concurrently; reload and retry")),
            None => ServerChangeError::Store(e),
        })?;
        self.reload_locked().await.map_err(ServerChangeError::Store)?;
        Ok(())
    }

    /// Remove a server stored in the vault, with its secrets and login.
    /// Refused while a policy lists it by name.
    pub async fn delete_server(&self, name: &str, policies: &[Arc<Policy>]) -> Result<(), ServerChangeError> {
        let _changes = self.changes.lock().await;
        if self.cfg.servers.contains_key(name) {
            return Err(ServerChangeError::Conflict(format!("MCP server {name} is defined in the config file; remove it there")));
        }
        let key = format!("{SERVERS_PREFIX}/{name}");
        if self.secrets.get(&key).await.map_err(ServerChangeError::Store)?.is_none() {
            return Err(ServerChangeError::NotFound(name.to_string()));
        }
        let users: Vec<&str> = policies.iter().filter(|p| p.mcp_servers.iter().any(|s| s == name)).map(|p| p.name.as_str()).collect();
        if !users.is_empty() {
            return Err(ServerChangeError::Conflict(format!(
                "MCP server {name} is used by polic{} {}; remove it from them first",
                if users.len() == 1 { "y" } else { "ies" },
                users.join(", ")
            )));
        }
        // Stop the login first, so nothing in flight writes it back.
        if let Some(store) = self.get(name).and_then(|u| u.oauth.clone()) {
            store.retire().await;
        }
        self.flows.cancel_server(name);
        self.secrets.delete(&key).await.map_err(ServerChangeError::Store)?;
        for secret in [format!("mcp/{name}/oauth"), format!("mcp/{name}")] {
            if let Err(e) = self.secrets.delete(&secret).await {
                tracing::warn!(secret = %secret, error = %format!("{e:#}"), "removing a deleted MCP server's secret failed");
            }
        }
        self.reload_locked().await.map_err(ServerChangeError::Store)?;
        Ok(())
    }

    // ---- OAuth logins

    fn oauth_server(&self, name: &str) -> Result<(Arc<Upstream>, Arc<oauth::OAuthStore>)> {
        let up = self.server(name)?;
        let store = up.oauth.clone().ok_or_else(|| anyhow::anyhow!("MCP server {name} does not use OAuth (auth = \"oauth\")"))?;
        Ok((up, store))
    }

    /// Start an OAuth login to a server.
    pub async fn login_start(&self, name: &str) -> Result<oauth::FlowStart> {
        let (up, store) = self.oauth_server(name)?;
        let start = oauth::start(&self.app, name, &up.cfg, &store, &self.secrets, up.http(), &self.flows);
        tokio::time::timeout(std::time::Duration::from_secs(60), start)
            .await
            .map_err(|_| anyhow::anyhow!("discovery and client registration timed out after 60s"))?
    }

    /// Finish a login with the URL the browser was redirected to, then
    /// reconnect and re-read the server's tools.
    pub async fn login_complete(&self, flow_id: &str, redirect_url: &str) -> Result<String> {
        let server = self.flows.complete(flow_id, redirect_url, |name| self.get(name).and_then(|u| u.oauth.clone())).await?;
        let up = self.server(&server)?;
        up.disconnect().await;
        if let Err(e) = up.refresh().await {
            tracing::warn!(server = %server, error = %e, "signed in, but listing tools failed");
        }
        tracing::info!(server = %server, "MCP OAuth login complete");
        Ok(server)
    }

    pub async fn logout(&self, name: &str) -> Result<()> {
        let (up, store) = self.oauth_server(name)?;
        self.flows.cancel_server(name);
        store.sign_out().await?;
        up.disconnect().await;
        Ok(())
    }

    pub fn client_metadata(&self) -> Option<Value> {
        oauth::client_metadata(&self.app)
    }

    pub async fn unsaved_count(&self) -> usize {
        let mut n = 0;
        for up in self.upstreams() {
            if let Some(store) = &up.oauth {
                n += usize::from(store.is_unsaved().await);
            }
        }
        n
    }

    /// Retry vault writes of OAuth credentials that failed.
    pub async fn flush_unsaved(&self) {
        for up in self.upstreams() {
            if let Some(store) = &up.oauth {
                store.flush().await;
            }
        }
    }

    // ---- reading

    pub fn server_names(&self) -> BTreeSet<String> {
        self.servers.read().keys().cloned().collect()
    }

    pub fn has_server(&self, name: &str) -> bool {
        self.servers.read().contains_key(name)
    }

    pub async fn servers(&self) -> Result<Vec<ServerInfo>> {
        let mut out = Vec::new();
        for (name, Entry { up, source }) in self.entries() {
            let secret = format!("mcp/{name}");
            let mut fields = BTreeSet::new();
            for v in up.cfg.headers.values() {
                fields.extend(template_fields(v)?);
            }
            if up.cfg.auth == McpAuth::Oauth && up.cfg.oauth.client_id.is_some() {
                fields.insert("client_secret".to_string());
            }
            let stored = if fields.is_empty() { Default::default() } else { self.secrets.get(&secret).await?.unwrap_or_default() };
            let secret_fields = fields
                .into_iter()
                .map(|f| {
                    let set = stored.get(&f).and_then(Value::as_str).is_some_and(|v| !v.is_empty());
                    (f, set)
                })
                .collect();
            out.push(ServerInfo {
                url: up.cfg.url.clone(),
                description: up.cfg.description.clone(),
                source,
                definition: (source == ServerSource::Vault).then(|| up.cfg.clone()),
                namespace: namespace(&name),
                secret,
                secret_fields,
                status: up.status().await,
                name,
            });
        }
        Ok(out)
    }

    /// Every tool of one server, unfiltered (admin view).
    pub fn server_tools(&self, name: &str) -> Option<Arc<Vec<Tool>>> {
        self.get(name).map(|u| u.tools())
    }

    /// Merge fields into a server's vault secret and reconnect with them.
    pub async fn set_secret(&self, name: &str, fields: Map<String, Value>) -> Result<()> {
        let up = self.server(name)?;
        let key = format!("mcp/{name}");
        let mut data = self.secrets.get(&key).await?.unwrap_or_default();
        for (k, v) in fields {
            match v {
                Value::Null => {
                    data.remove(&k);
                }
                Value::String(s) => {
                    data.insert(k, Value::String(s));
                }
                _ => anyhow::bail!("secret field {k:?} must be a string (or null to remove it)"),
            }
        }
        if data.is_empty() {
            self.secrets.delete(&key).await?;
        } else {
            self.secrets.put(&key, data).await?;
        }
        up.disconnect().await;
        up.retry_listing_now();
        Ok(())
    }

    /// Re-read every server's tool list. Failures are logged and kept per server.
    pub async fn refresh_all(&self) {
        let ups = self.upstreams();
        let jobs = ups.iter().map(|up| async move {
            match up.refresh().await {
                Ok(n) => tracing::debug!(server = %up.name, tools = n, "refreshed MCP tool list"),
                Err(e) => tracing::warn!(server = %up.name, error = %e, "MCP tool list refresh failed"),
            }
        });
        futures::future::join_all(jobs).await;
    }

    pub async fn refresh(&self, name: &str) -> Result<usize> {
        self.server(name)?.refresh().await
    }

    /// The servers a policy may use, in name order.
    pub fn allowed_servers(&self, policy: &Policy) -> Vec<(String, McpServerConfig)> {
        self.servers.read().iter().filter(|(n, _)| policy.allows_mcp_server(n)).map(|(n, e)| (n.clone(), e.up.cfg.clone())).collect()
    }

    /// The tools a policy may call. Servers whose tools were never listed
    /// are asked first (see [`Upstream::needs_listing`]).
    pub async fn allowed_tools(&self, policy: &Policy) -> Vec<AllowedTool> {
        let ups: Vec<Arc<Upstream>> = self.upstreams().into_iter().filter(|u| policy.allows_mcp_server(&u.name)).collect();
        futures::future::join_all(ups.iter().filter(|u| u.needs_listing()).map(|u| u.refresh())).await;
        ups.iter()
            .flat_map(|u| {
                u.tools()
                    .iter()
                    .filter(|t| policy.allows_tool(&u.name, &t.name))
                    .map(|t| AllowedTool { server: u.name.clone(), tool: t.clone() })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Call an upstream tool for a principal: policy check, call log, metrics.
    pub async fn call(&self, principal: &Principal, server: &str, tool: &str, arguments: JsonObject) -> Result<CallToolResult, String> {
        let up = self.get(server).filter(|_| principal.policy.allows_mcp_server(server));
        let Some(up) = up else {
            return Err(format!("unknown MCP server {server:?}"));
        };
        if !principal.policy.allows_tool(server, tool) {
            return Err(format!("tool {server}/{tool} is not allowed for this key"));
        }
        if up.has_tool_list() && !up.tools().iter().any(|t| t.name == tool) {
            return Err(format!("{server} has no tool {tool:?}"));
        }
        let started = Instant::now();
        let result = up.call(tool, arguments).await;
        let latency = started.elapsed();
        let (status, error) = match &result {
            Ok(r) if r.is_error == Some(true) => ("tool_error", Some(text_of(r))),
            Ok(_) => ("ok", None),
            Err(e) => ("error", Some(e.to_string())),
        };
        let attrs = [KeyValue::new("server", server.to_string()), KeyValue::new("tool", tool.to_string()), KeyValue::new("status", status)];
        metrics().mcp_tool_calls.add(1, &attrs);
        metrics().mcp_tool_call_duration.record(latency.as_secs_f64(), &attrs[..2]);
        let record = McpCallRecord {
            client_id: principal.client_id().map(str::to_string),
            server: server.to_string(),
            tool: tool.to_string(),
            status: status.to_string(),
            error: error.map(|e| truncate(&e, 500)),
            latency_ms: latency.as_millis() as i64,
            trace_id: crate::telemetry::current_trace_id(),
        };
        if let Err(e) = self.db.log_mcp_call(&record).await {
            tracing::warn!(error = %e, "writing MCP call log failed");
        }
        result.map_err(|e| format!("{server}/{tool}: {e}"))
    }

    /// Wait for an execution slot.
    pub async fn execution_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.executions.clone().acquire_owned().await.expect("semaphore is never closed")
    }
}

/// The text blocks of a tool result, joined.
pub fn text_of(r: &CallToolResult) -> String {
    let texts: Vec<&str> = r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect();
    if texts.is_empty() {
        r.structured_content.as_ref().map(Value::to_string).unwrap_or_else(|| "tool reported an error".into())
    } else {
        texts.join("\n")
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn templates() {
        assert_eq!(template_fields("Bearer {token}").unwrap(), vec!["token"]);
        assert_eq!(template_fields("plain").unwrap(), Vec::<String>::new());
        for bad in ["{", "}", "{}", "{a b}", "x}{y"] {
            assert!(template_fields(bad).is_err(), "{bad}");
        }
        let secret = json!({"token": "t0k", "org": "acme"}).as_object().unwrap().clone();
        assert_eq!(render_template("Bearer {token} ({org})", &secret).unwrap(), "Bearer t0k (acme)");
        assert!(render_template("{missing}", &secret).unwrap_err().to_string().contains("missing"));
    }
}
