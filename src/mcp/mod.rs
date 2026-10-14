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
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::{Config, McpAuth, McpConfig};
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

/// An upstream server as the admin API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub url: String,
    pub description: String,
    /// Name used in code mode (`-` becomes `_`).
    pub namespace: String,
    /// Vault secret holding the header placeholder values.
    pub secret: String,
    /// Placeholder fields the headers use, and whether each is set.
    pub secret_fields: BTreeMap<String, bool>,
    #[serde(flatten)]
    pub status: UpstreamStatus,
}

pub struct McpHub {
    pub cfg: McpConfig,
    /// The whole config, for the OAuth redirect URI (`server.public_url`).
    app: Config,
    secrets: Secrets,
    pub flows: oauth::Flows,
    db: Db,
    servers: BTreeMap<String, Arc<Upstream>>,
    executions: tokio::sync::Semaphore,
}

/// The JavaScript identifier of a server name.
pub fn namespace(server: &str) -> String {
    server.replace('-', "_")
}

impl McpHub {
    pub fn new(app: &Config, secrets: Secrets, db: Db) -> Self {
        let cfg = &app.mcp;
        let servers =
            cfg.servers.iter().map(|(name, s)| (name.clone(), Arc::new(Upstream::new(name.clone(), s.clone(), secrets.clone())))).collect();
        Self {
            cfg: cfg.clone(),
            app: app.clone(),
            secrets,
            flows: Default::default(),
            db,
            servers,
            executions: tokio::sync::Semaphore::new(cfg.max_concurrent_executions.max(1)),
        }
    }

    fn server(&self, name: &str) -> Result<&Arc<Upstream>> {
        self.servers.get(name).ok_or_else(|| anyhow::anyhow!("no such MCP server"))
    }

    fn oauth_server(&self, name: &str) -> Result<(&Arc<Upstream>, &Arc<oauth::OAuthStore>)> {
        let up = self.server(name)?;
        let store = up.oauth.as_ref().ok_or_else(|| anyhow::anyhow!("MCP server {name} does not use OAuth (auth = \"oauth\")"))?;
        Ok((up, store))
    }

    /// Start an OAuth login to a server.
    pub async fn login_start(&self, name: &str) -> Result<oauth::FlowStart> {
        let (up, store) = self.oauth_server(name)?;
        let start = oauth::start(&self.app, name, &up.cfg, store, &self.secrets, up.http(), &self.flows);
        tokio::time::timeout(std::time::Duration::from_secs(60), start)
            .await
            .map_err(|_| anyhow::anyhow!("discovery and client registration timed out after 60s"))?
    }

    /// Finish a login with the URL the browser was redirected to, then
    /// reconnect and re-read the server's tools.
    pub async fn login_complete(&self, flow_id: &str, redirect_url: &str) -> Result<String> {
        let server = self.flows.complete(flow_id, redirect_url).await?;
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
        store.sign_out().await?;
        up.disconnect().await;
        Ok(())
    }

    pub fn client_metadata(&self) -> Option<Value> {
        oauth::client_metadata(&self.app)
    }

    pub async fn unsaved_count(&self) -> usize {
        let mut n = 0;
        for store in self.servers.values().filter_map(|u| u.oauth.as_ref()) {
            n += usize::from(store.is_unsaved().await);
        }
        n
    }

    /// Retry vault writes of OAuth credentials that failed.
    pub async fn flush_unsaved(&self) {
        for store in self.servers.values().filter_map(|u| u.oauth.as_ref()) {
            store.flush().await;
        }
    }

    pub fn server_names(&self) -> BTreeSet<String> {
        self.servers.keys().cloned().collect()
    }

    pub fn has_server(&self, name: &str) -> bool {
        self.servers.contains_key(name)
    }

    pub async fn servers(&self) -> Result<Vec<ServerInfo>> {
        let mut out = Vec::new();
        for (name, up) in &self.servers {
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
                name: name.clone(),
                url: up.cfg.url.clone(),
                description: up.cfg.description.clone(),
                namespace: namespace(name),
                secret,
                secret_fields,
                status: up.status().await,
            });
        }
        Ok(out)
    }

    /// Every tool of one server, unfiltered (admin view).
    pub fn server_tools(&self, name: &str) -> Option<Arc<Vec<Tool>>> {
        self.servers.get(name).map(|u| u.tools())
    }

    /// Merge fields into a server's vault secret and reconnect with them.
    pub async fn set_secret(&self, name: &str, fields: Map<String, Value>) -> Result<()> {
        let up = self.servers.get(name).ok_or_else(|| anyhow::anyhow!("no such MCP server"))?;
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
        Ok(())
    }

    /// Re-read every server's tool list. Failures are logged and kept per server.
    pub async fn refresh_all(&self) {
        let jobs = self.servers.values().map(|up| async move {
            match up.refresh().await {
                Ok(n) => tracing::debug!(server = %up.name, tools = n, "refreshed MCP tool list"),
                Err(e) => tracing::warn!(server = %up.name, error = %e, "MCP tool list refresh failed"),
            }
        });
        futures::future::join_all(jobs).await;
    }

    pub async fn refresh(&self, name: &str) -> Result<usize> {
        self.servers.get(name).ok_or_else(|| anyhow::anyhow!("no such MCP server"))?.refresh().await
    }

    /// The servers a policy may use, in name order.
    pub fn allowed_servers(&self, policy: &Policy) -> Vec<(&str, &crate::config::McpServerConfig)> {
        self.servers.iter().filter(|(n, _)| policy.allows_mcp_server(n)).map(|(n, u)| (n.as_str(), &u.cfg)).collect()
    }

    /// The tools a policy may call. Servers whose tools were never listed
    /// are asked first (see [`Upstream::needs_listing`]).
    pub async fn allowed_tools(&self, policy: &Policy) -> Vec<AllowedTool> {
        let pending = self.servers.values().filter(|u| policy.allows_mcp_server(&u.name) && u.needs_listing()).map(|u| u.refresh());
        futures::future::join_all(pending).await;
        self.servers
            .iter()
            .filter(|(n, _)| policy.allows_mcp_server(n))
            .flat_map(|(n, u)| {
                u.tools()
                    .iter()
                    .filter(|t| policy.allows_tool(n, &t.name))
                    .map(|t| AllowedTool { server: n.clone(), tool: t.clone() })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Call an upstream tool for a principal: policy check, call log, metrics.
    pub async fn call(&self, principal: &Principal, server: &str, tool: &str, arguments: JsonObject) -> Result<CallToolResult, String> {
        let up = self.servers.get(server).filter(|_| principal.policy.allows_mcp_server(server));
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
    pub async fn execution_permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.executions.acquire().await.expect("semaphore is never closed")
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
