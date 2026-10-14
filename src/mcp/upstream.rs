//! One upstream MCP server reached over Streamable HTTP: a lazily opened
//! client connection and the server's cached tool list.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::http::{HeaderName, HeaderValue};
use parking_lot::RwLock;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig, Implementation, JsonObject, ProtocolVersion,
    Tool,
};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde::Serialize;

use crate::config::McpServerConfig;
use crate::db::now;
use crate::secrets::Secrets;

/// Time allowed to open a connection (discovery or `initialize`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// What the admin API shows about an upstream server.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpstreamStatus {
    pub connected: bool,
    /// The upstream's name and version, from its discovery or initialize result.
    pub server_info: Option<String>,
    pub protocol_version: Option<String>,
    pub tools: usize,
    pub tools_refreshed_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Cached {
    tools: Arc<Vec<Tool>>,
    refreshed_at: Option<i64>,
    /// Last listing attempt, successful or not.
    attempted_at: Option<std::time::Instant>,
    server_info: Option<String>,
    protocol_version: Option<String>,
    last_error: Option<String>,
}

pub struct Upstream {
    pub name: String,
    pub cfg: McpServerConfig,
    secrets: Secrets,
    http: reqwest::Client,
    client: tokio::sync::Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
    cached: RwLock<Cached>,
}

/// Why a tool call failed before the tool produced a result.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("{0}")]
    Connect(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Upstream(String),
}

impl Upstream {
    pub fn new(name: String, cfg: McpServerConfig, secrets: Secrets) -> Self {
        // No redirects: the headers carry credentials meant for this origin only.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        Self { name, cfg, secrets, http, client: Default::default(), cached: Default::default() }
    }

    pub fn status(&self) -> UpstreamStatus {
        let c = self.cached.read();
        let connected = self.client.try_lock().map(|g| g.as_ref().is_some_and(|s| !s.is_closed())).unwrap_or(false);
        UpstreamStatus {
            connected,
            server_info: c.server_info.clone(),
            protocol_version: c.protocol_version.clone(),
            tools: c.tools.len(),
            tools_refreshed_at: c.refreshed_at,
            last_error: c.last_error.clone(),
        }
    }

    pub fn tools(&self) -> Arc<Vec<Tool>> {
        self.cached.read().tools.clone()
    }

    pub fn has_tool_list(&self) -> bool {
        self.cached.read().refreshed_at.is_some()
    }

    /// A server never listed is asked again when a client needs it, but at
    /// most every 30 s, so an unreachable server doesn't slow every call.
    pub fn needs_listing(&self) -> bool {
        let c = self.cached.read();
        c.refreshed_at.is_none() && c.attempted_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
    }

    fn set_error(&self, e: &str) {
        self.cached.write().last_error = Some(e.to_string());
    }

    /// Drop the connection; the next use reconnects with fresh secrets.
    pub async fn disconnect(&self) {
        if let Some(svc) = self.client.lock().await.take() {
            let _ = svc.cancel().await;
        }
    }

    /// Headers with `{field}` placeholders filled from the vault secret `mcp/<name>`.
    async fn headers(&self) -> Result<HashMap<HeaderName, HeaderValue>> {
        let secret = if self.cfg.headers.values().any(|v| v.contains('{')) {
            self.secrets.get(&format!("mcp/{}", self.name)).await.context("reading the server's vault secret")?.unwrap_or_default()
        } else {
            Default::default()
        };
        let mut out = HashMap::new();
        for (k, v) in &self.cfg.headers {
            let value = super::render_template(v, &secret).with_context(|| format!("header {k}"))?;
            let mut value = HeaderValue::from_str(&value).with_context(|| format!("header {k} is not a valid header value"))?;
            value.set_sensitive(true);
            out.insert(HeaderName::from_bytes(k.as_bytes())?, value);
        }
        Ok(out)
    }

    async fn connect(&self) -> Result<RunningService<RoleClient, ClientConfig>> {
        let config = StreamableHttpClientTransportConfig::with_uri(self.cfg.url.clone())
            .custom_headers(self.headers().await?)
            .reinit_on_expired_session(true);
        let transport = StreamableHttpClientTransport::with_client(self.http.clone(), config);
        let info = ClientConfig::new(ClientCapabilities::default(), Implementation::new("xolotl", env!("CARGO_PKG_VERSION")));
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let svc = tokio::time::timeout(CONNECT_TIMEOUT, info.serve_with_lifecycle(transport, lifecycle))
            .await
            .map_err(|_| anyhow::anyhow!("connecting timed out after {CONNECT_TIMEOUT:?}"))?
            .map_err(|e| anyhow::anyhow!("connecting failed: {e}"))?;
        let mut c = self.cached.write();
        if let Some(info) = svc.peer_info() {
            c.server_info = serde_json::to_value(&*info).ok().and_then(|v| {
                let si = v.get("serverInfo")?;
                Some(
                    format!("{} {}", si.get("name")?.as_str()?, si.get("version").and_then(|v| v.as_str()).unwrap_or(""))
                        .trim()
                        .to_string(),
                )
            });
            c.protocol_version =
                serde_json::to_value(&*info).ok().and_then(|v| v.get("protocolVersion").and_then(|p| p.as_str()).map(str::to_string));
        }
        Ok(svc)
    }

    /// The open connection's peer, connecting first when there is none or it closed.
    async fn peer(&self) -> Result<Peer<RoleClient>, CallError> {
        let mut guard = self.client.lock().await;
        if let Some(svc) = guard.as_ref()
            && !svc.is_closed()
        {
            return Ok(svc.peer().clone());
        }
        match self.connect().await {
            Ok(svc) => {
                let peer = svc.peer().clone();
                *guard = Some(svc);
                Ok(peer)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.set_error(&msg);
                Err(CallError::Connect(msg))
            }
        }
    }

    /// Re-read the tool list. On failure the previous list is kept.
    pub async fn refresh(&self) -> Result<usize> {
        self.cached.write().attempted_at = Some(std::time::Instant::now());
        let result = async {
            let peer = self.peer().await?;
            match tokio::time::timeout(self.cfg.timeout(), peer.list_all_tools()).await {
                Ok(Ok(tools)) => Ok(tools),
                Ok(Err(e)) => Err(self.upstream_error(e).await),
                Err(_) => Err(CallError::Timeout(self.cfg.timeout())),
            }
        }
        .await;
        match result {
            Ok(tools) => {
                let n = tools.len();
                let mut c = self.cached.write();
                c.tools = Arc::new(tools);
                c.refreshed_at = Some(now());
                c.last_error = None;
                Ok(n)
            }
            Err(e) => {
                let msg = format!("listing tools: {e}");
                self.set_error(&msg);
                Err(anyhow::anyhow!(msg))
            }
        }
    }

    /// Call a tool. A result with `isError` is still `Ok`: the tool ran.
    pub async fn call(&self, tool: &str, arguments: JsonObject) -> Result<CallToolResult, CallError> {
        let peer = self.peer().await?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(arguments);
        let timeout = self.cfg.timeout();
        match tokio::time::timeout(timeout, peer.call_tool_once(params)).await {
            Ok(Ok(CallToolResponse::Complete(r))) => Ok(r),
            Ok(Ok(CallToolResponse::InputRequired(_))) => {
                Err(CallError::Upstream("the tool asked for user input, which the gateway does not support".into()))
            }
            Ok(Ok(_)) => Err(CallError::Upstream("the tool returned a task, which the gateway does not support".into())),
            Ok(Err(e)) => Err(self.upstream_error(e).await),
            Err(_) => Err(CallError::Timeout(timeout)),
        }
    }

    /// A transport failure drops the connection so the next call reconnects.
    async fn upstream_error(&self, e: ServiceError) -> CallError {
        if matches!(e, ServiceError::TransportClosed | ServiceError::TransportSend(_)) {
            self.disconnect().await;
            self.set_error(&e.to_string());
        }
        CallError::Upstream(e.to_string())
    }
}
