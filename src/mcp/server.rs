//! The code-mode MCP server clients connect to at `/mcp`: two tools,
//! `search` and `execute`, over the tools the caller's policy allows.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{Value, json};

use super::codemode::{self, Group, Limits, ToolHost};
use super::{AllowedTool, McpHub, namespace, text_of, truncate};
use crate::keys::Principal;
use crate::telemetry::metrics;

/// Most tools `search` describes in full; further matches are only named.
const SEARCH_LIMIT: usize = 20;

const INSTRUCTIONS: &str = "This server is a gateway to other MCP servers in code mode. \
Call `search` to find tools: it returns TypeScript declarations in which every server is a global object \
and every tool an async method. Then call `execute` with JavaScript that calls them, e.g. \
`const issue = await github.create_issue({title: 'x'}); return issue.number;`. \
Do the filtering and joining in the code and return only what you need.";

#[derive(Clone)]
pub struct CodeModeServer {
    hub: Arc<McpHub>,
}

impl CodeModeServer {
    pub fn new(hub: Arc<McpHub>) -> Self {
        Self { hub }
    }
}

/// The caller, put into the request's extensions by the `/mcp` auth middleware.
fn principal(ctx: &RequestContext<RoleServer>) -> Result<Principal, ErrorData> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Principal>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("request was not authenticated", None))
}

fn object(v: Value) -> Arc<JsonObject> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => unreachable!("schemas are objects"),
    }
}

impl CodeModeServer {
    fn tools(&self, principal: &Principal) -> Vec<Tool> {
        let servers = self.hub.allowed_servers(&principal.policy);
        let list = if servers.is_empty() {
            "No MCP servers are available to this key.".to_string()
        } else {
            servers
                .iter()
                .map(|(n, s)| {
                    if s.description.is_empty() {
                        format!("- `{}`", namespace(n))
                    } else {
                        format!("- `{}`: {}", namespace(n), s.description)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let search = Tool::new(
            "search",
            "Find tools of the connected MCP servers. Returns TypeScript declarations: each server is a global object, each tool an async \
             method taking one argument object. Words in the query must all appear in the server name, tool name or description; an empty \
             query lists every tool.",
            object(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for, e.g. \"issue create\". Empty lists everything."}
                },
                "required": ["query"],
                "additionalProperties": false
            })),
        )
        .annotate(ToolAnnotations::new().read_only(true).open_world(false));
        let execute = Tool::new(
            "execute",
            format!(
                "Run JavaScript (QuickJS, no network, files or timers) as the body of an async function. Call tools as \
                 `await server.tool({{...}})` with the names from `search` (or `await codemode.call(\"server\", \"tool\", {{...}})` \
                 for any name), and `return` a JSON-serializable result. \
                 A tool resolves to its structured content when it has any, otherwise to its text (several text blocks are \
                 joined with newlines); a tool error is thrown. console.log output is returned too. Limits: {}s, {} tool calls.\n\n\
                 Servers:\n{list}",
                self.hub.cfg.execute_timeout_secs, self.hub.cfg.max_tool_calls
            ),
            object(json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Body of an async function, e.g. `const r = await github.list_issues({}); return r.length;`"}
                },
                "required": ["code"],
                "additionalProperties": false
            })),
        )
        .annotate(ToolAnnotations::new().destructive(true).open_world(true));
        vec![search, execute]
    }

    async fn search(&self, principal: &Principal, query: &str) -> CallToolResult {
        let tools = self.hub.allowed_tools(&principal.policy).await;
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let matches: Vec<&AllowedTool> = tools
            .iter()
            .filter(|t| {
                let haystack =
                    format!("{} {} {} {}", t.server, namespace(&t.server), t.tool.name, t.tool.description.as_deref().unwrap_or(""))
                        .to_lowercase();
                words.iter().all(|w| haystack.contains(w))
            })
            .collect();
        if matches.is_empty() {
            let msg =
                if tools.is_empty() { "No tools are available to this key." } else { "No tools match; try fewer words or an empty query." };
            return CallToolResult::success(vec![ContentBlock::text(msg)]);
        }
        let (shown, rest) = matches.split_at(matches.len().min(SEARCH_LIMIT));
        let mut by_server: BTreeMap<&str, Vec<&rmcp::model::Tool>> = BTreeMap::new();
        for t in shown {
            by_server.entry(t.server.as_str()).or_default().push(&t.tool);
        }
        let namespaces: Vec<String> = by_server.keys().map(|s| namespace(s)).collect();
        let descriptions: BTreeMap<String, String> =
            self.hub.allowed_servers(&principal.policy).into_iter().map(|(n, c)| (n, c.description)).collect();
        let groups: Vec<Group<'_>> = by_server
            .iter()
            .zip(&namespaces)
            .map(|((server, tools), ns)| Group {
                namespace: ns,
                server,
                description: descriptions.get(*server).map(String::as_str).unwrap_or(""),
                tools: tools.clone(),
            })
            .collect();
        let mut text = codemode::declarations(&groups);
        if !rest.is_empty() {
            let names: Vec<String> = rest.iter().map(|t| format!("{}.{}", namespace(&t.server), t.tool.name)).collect();
            text.push_str(&format!("\n\n// {} more matches; search for one by name to see it: {}", rest.len(), names.join(", ")));
        }
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    async fn execute(&self, principal: Principal, code: &str) -> CallToolResult {
        let permit = self.hub.execution_permit().await;
        let tools = self.hub.allowed_tools(&principal.policy).await;
        let namespaces: Vec<(String, String)> =
            self.hub.allowed_servers(&principal.policy).iter().map(|(n, _)| (namespace(n), n.clone())).collect();
        let cfg = &self.hub.cfg;
        let limits = Limits {
            timeout: Duration::from_secs(cfg.execute_timeout_secs.max(1)),
            memory_bytes: cfg.memory_limit_mb.max(1) << 20,
            max_calls: cfg.max_tool_calls,
            max_log_bytes: cfg.max_output_bytes / 2,
        };
        let host = Arc::new(Host { hub: self.hub.clone(), principal, tools });
        let outcome = codemode::execute(code, namespaces, host, limits, Some(permit)).await;
        let outcome_label = if outcome.result.is_ok() { "ok" } else { "error" };
        metrics().mcp_executions.add(1, &[KeyValue::new("status", outcome_label)]);
        let (body, is_error) = match outcome.result {
            Ok(result) => (json!({"result": result, "logs": outcome.logs, "calls": outcome.calls}), false),
            Err(error) => (json!({"error": error, "logs": outcome.logs, "calls": outcome.calls}), true),
        };
        let mut text = serde_json::to_string_pretty(&body).expect("values serialize");
        if text.len() > cfg.max_output_bytes {
            text = format!(
                "{}\n[output truncated at {} bytes: return less data]",
                truncate(&text, cfg.max_output_bytes),
                cfg.max_output_bytes
            );
        }
        let content = vec![ContentBlock::text(text)];
        if is_error { CallToolResult::error(content) } else { CallToolResult::success(content) }
    }
}

impl ServerHandler for CodeModeServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("xolotl", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(INSTRUCTIONS.into());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let principal = principal(&ctx)?;
        Ok(ListToolsResult::with_all_items(self.tools(&principal)))
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let principal = principal(&ctx)?;
        let args = request.arguments.unwrap_or_default();
        let string = |k: &str| -> Result<String, ErrorData> {
            match args.get(k) {
                Some(Value::String(s)) => Ok(s.clone()),
                None if k == "query" => Ok(String::new()),
                _ => Err(ErrorData::invalid_params(format!("`{k}` must be a string"), None)),
            }
        };
        let result = match request.name.as_ref() {
            "search" => self.search(&principal, &string("query")?).await,
            "execute" => self.execute(principal, &string("code")?).await,
            other => return Err(ErrorData::invalid_params(format!("unknown tool {other:?}"), None)),
        };
        Ok(CallToolResponse::Complete(result))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [rmcp::model::ProtocolVersion]> {
        Cow::Borrowed(rmcp::model::ProtocolVersion::KNOWN_VERSIONS)
    }
}

/// Tool calls from one execution, checked against the caller's policy.
struct Host {
    hub: Arc<McpHub>,
    principal: Principal,
    /// The allowed tools when the execution started.
    tools: Vec<AllowedTool>,
}

/// A tool name as a JavaScript identifier, the form a model may guess for
/// names with `-` or `.`.
fn identifier(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '$' { c } else { '_' }).collect()
}

#[async_trait::async_trait]
impl ToolHost for Host {
    async fn call(&self, server: String, tool: String, arguments: Value) -> Result<Value, String> {
        let on_server = || self.tools.iter().filter(|t| t.server == server);
        let name = on_server()
            .find(|t| t.tool.name == tool)
            .or_else(|| on_server().find(|t| identifier(&t.tool.name) == tool))
            .map(|t| t.tool.name.to_string())
            .ok_or_else(|| format!("{}.{tool} is not a tool available to this key; use search to find tools", namespace(&server)))?;
        let Value::Object(arguments) = arguments else {
            return Err("tool arguments must be an object".into());
        };
        let result = self.hub.call(&self.principal, &server, &name, arguments).await?;
        if result.is_error == Some(true) {
            return Err(text_of(&result));
        }
        Ok(tool_value(result))
    }
}

/// What a tool call resolves to in JavaScript.
fn tool_value(r: CallToolResult) -> Value {
    if let Some(v) = r.structured_content {
        return v;
    }
    let texts: Option<Vec<&str>> = r.content.iter().map(|c| c.as_text().map(|t| t.text.as_str())).collect();
    match texts {
        Some(t) if !t.is_empty() => Value::String(t.join("\n")),
        Some(_) => Value::Null,
        None => serde_json::to_value(&r.content).unwrap_or(Value::Null),
    }
}
