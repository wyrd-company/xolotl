//! Admin API used by the `xolotl` CLI: client keys, accounts, logins, usage.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AppState, authorize, json_error};
use crate::db::now;
use crate::keys::{IssueRequest, Scope};
use crate::policy::Policy;
use crate::secrets::SecretStore;
use crate::subscription::SubProvider;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/policies", get(list_policies))
        .route("/policies/{name}", get(get_policy).put(put_policy).delete(delete_policy))
        .route("/policies/{name}/view", get(policy_view))
        // Not under /policies/: `preview` is a valid policy name.
        .route("/policy-preview", post(preview_policy))
        .route("/catalog", get(catalog))
        .route("/catalog/routes", get(catalog_routes))
        .route("/catalog/refresh", post(refresh_catalog))
        .route("/keys", get(list_keys).post(issue_key))
        .route("/keys/{policy}/{name}", delete(revoke_key))
        .route("/keys/{policy}/{name}/rotate", post(rotate_key))
        .route("/keys/{policy}/{name}/enable", post(|s, h, p| set_key_disabled(s, h, p, false)))
        .route("/keys/{policy}/{name}/disable", post(|s, h, p| set_key_disabled(s, h, p, true)))
        .route("/accounts", get(list_accounts))
        .route("/accounts/import", post(import_account))
        .route("/accounts/{id}", delete(remove_account))
        .route("/accounts/{id}/enable", post(|s, h, p| set_account_enabled(s, h, p, true)))
        .route("/accounts/{id}/disable", post(|s, h, p| set_account_enabled(s, h, p, false)))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/accounts/{id}/priority", post(set_priority))
        .route("/accounts/{id}/usage", post(refresh_usage))
        .route("/auth/{provider}/start", post(start_login))
        .route("/auth/flows/{id}", get(flow_status))
        .route("/auth/flows/{id}/complete", post(complete_login))
        .route("/providers", get(list_providers))
        .route("/providers/{name}/key", post(set_provider_key))
        .route("/usage", get(usage))
        .route("/mcp/servers", get(mcp_servers).post(mcp_create_server))
        .route("/mcp/servers/{name}", axum::routing::put(mcp_update_server).delete(mcp_delete_server))
        .route("/mcp/servers/{name}/tools", get(mcp_tools))
        .route("/mcp/servers/{name}/refresh", post(mcp_refresh))
        .route("/mcp/servers/{name}/secret", axum::routing::put(mcp_set_secret))
        .route("/mcp/servers/{name}/oauth", delete(mcp_logout))
        .route("/mcp/servers/{name}/oauth/start", post(mcp_login_start))
        .route("/mcp/oauth/flows/{id}", get(mcp_login_status))
        .route("/mcp/oauth/flows/{id}/complete", post(mcp_login_complete))
        // Public: the authorization server redirects the browser here, and
        // authorization servers fetch the client metadata document.
        .route("/mcp/oauth/callback", get(mcp_login_callback))
        .route("/mcp/oauth/client-metadata.json", get(mcp_client_metadata))
        .route("/mcp/refresh", post(mcp_refresh_all))
        .route("/mcp/usage", get(mcp_usage))
        .route("/mcp/calls", get(mcp_calls))
        .route("/reload", post(reload))
        .route("/status", get(status))
}

type Res = Result<Response, Response>;

async fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    authorize(state, headers, Scope::Admin).await.map(|_| ())
}

fn internal(e: anyhow::Error) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(e: anyhow::Error) -> Response {
    json_error(StatusCode::BAD_REQUEST, format!("{e:#}"))
}

fn ok(v: impl serde::Serialize) -> Res {
    Ok(Json(v).into_response())
}

// ---- policies

async fn list_policies(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    ok(s.keys.policies().iter().map(|p| p.as_ref().clone()).collect::<Vec<_>>())
}

async fn get_policy(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    ok(s.keys.policy(&name).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such policy"))?.as_ref().clone())
}

async fn put_policy(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Json(mut policy): Json<Policy>) -> Res {
    admin(&s, &h).await?;
    if policy.name.is_empty() {
        policy.name = name.clone();
    }
    if policy.name != name {
        return Err(json_error(StatusCode::BAD_REQUEST, "policy name must match the URL"));
    }
    let saved = s.keys.put_policy(policy, &s.catalog.known_providers(), &s.mcp.server_names()).await.map_err(bad)?;
    tracing::info!(policy = %saved.name, "saved policy");
    ok(saved.as_ref().clone())
}

async fn delete_policy(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    s.keys.delete_policy(&name).await.map_err(bad)?;
    ok(json!({"deleted": name}))
}

/// The resolved model names a policy exposes, with targets and reachable endpoints.
async fn policy_view(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    let policy = s.keys.policy(&name).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such policy"))?;
    ok(view_json(&s, &policy))
}

/// The view of an unsaved policy, for editing. Invalid policies get a 400
/// naming the problem.
async fn preview_policy(State(s): State<AppState>, h: HeaderMap, Json(mut policy): Json<Policy>) -> Res {
    admin(&s, &h).await?;
    policy.normalize();
    if policy.name.is_empty() {
        policy.name = "preview".into();
    }
    policy.validate(&s.catalog.known_providers(), &s.mcp.server_names()).map_err(bad)?;
    ok(view_json(&s, &policy))
}

fn view_json(s: &AppState, policy: &Policy) -> Value {
    let catalog = s.catalog.models();
    let view = policy.resolve(&catalog);
    let models: Vec<Value> = view
        .names
        .iter()
        .map(|(name, targets)| {
            let first = &targets[0];
            let (native, translated) = s.catalog.endpoints(&first.provider, &first.model);
            json!({
                "name": name,
                "targets": targets.iter().map(|t| t.qualified()).collect::<Vec<_>>(),
                "endpoints": native,
                "translated_endpoints": translated,
                "alias": policy.aliases.contains_key(name),
            })
        })
        .collect();
    json!({
        "policy": policy.name,
        "models": models,
        "shared": policy.shared_names(&catalog),
        "mcp_servers": s.mcp.allowed_servers(policy).into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "tools": mcp_view_tools(s, policy),
    })
}

/// The `server/tool` names a policy allows, from the cached tool lists.
fn mcp_view_tools(s: &AppState, policy: &Policy) -> Vec<String> {
    s.mcp
        .allowed_servers(policy)
        .iter()
        .flat_map(|(server, _)| {
            s.mcp
                .server_tools(server)
                .into_iter()
                .flat_map(|tools| tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>())
                .filter(|t| policy.allows_tool(server, t))
                .map(|t| format!("{server}/{t}"))
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---- catalog

async fn catalog(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    ok(s.catalog.models())
}

/// Every catalog model with the endpoints it is reachable on, natively and
/// through translation.
async fn catalog_routes(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    let rows: Vec<Value> = s
        .catalog
        .models()
        .iter()
        .flat_map(|(provider, models)| models.iter().map(move |m| (provider, m)))
        .map(|(provider, model)| {
            let (native, translated) = s.catalog.endpoints(provider, model);
            json!({"provider": provider, "model": model, "endpoints": native, "translated_endpoints": translated})
        })
        .collect();
    ok(rows)
}

async fn refresh_catalog(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    s.catalog.refresh(&s.subs, &s.gateway).await;
    ok(s.catalog.models())
}

/// Accounts changed: re-discover models in the background.
fn refresh_catalog_later(s: &AppState) {
    let s = s.clone();
    tokio::spawn(async move { s.catalog.refresh(&s.subs, &s.gateway).await });
}

// ---- keys

async fn list_keys(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    ok(s.keys.list())
}

/// Issues a key into the vault. The key itself is never returned: the client
/// reads it from `vault_path` with its own Vault policy.
async fn issue_key(State(s): State<AppState>, h: HeaderMap, Json(req): Json<IssueRequest>) -> Res {
    admin(&s, &h).await?;
    let info = s.keys.issue(req).await.map_err(bad)?;
    tracing::info!(client = %info.client_id, "issued client key");
    ok(info)
}

#[derive(Deserialize, Default)]
struct RotateReq {
    #[serde(default)]
    grace_secs: i64,
}

async fn rotate_key(
    State(s): State<AppState>,
    h: HeaderMap,
    Path((policy, name)): Path<(String, String)>,
    body: Option<Json<RotateReq>>,
) -> Res {
    admin(&s, &h).await?;
    let id = format!("{policy}/{name}");
    let grace = body.map(|b| b.0.grace_secs).unwrap_or(0);
    let info = s.keys.rotate(&id, grace).await.map_err(|e| json_error(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    tracing::info!(client = %id, grace, "rotated client key");
    ok(info)
}

async fn revoke_key(State(s): State<AppState>, h: HeaderMap, Path((policy, name)): Path<(String, String)>) -> Res {
    admin(&s, &h).await?;
    let id = format!("{policy}/{name}");
    s.keys.revoke(&id).await.map_err(|e| json_error(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    tracing::info!(client = %id, "revoked client key");
    ok(json!({"revoked": id}))
}

async fn set_key_disabled(State(s): State<AppState>, h: HeaderMap, Path((policy, name)): Path<(String, String)>, disabled: bool) -> Res {
    admin(&s, &h).await?;
    let id = format!("{policy}/{name}");
    ok(s.keys.set_disabled(&id, disabled).await.map_err(|e| json_error(StatusCode::NOT_FOUND, format!("{e:#}")))?)
}

// ---- accounts

#[derive(Deserialize)]
struct ProviderFilter {
    provider: Option<SubProvider>,
}

async fn list_accounts(State(s): State<AppState>, h: HeaderMap, Query(q): Query<ProviderFilter>) -> Res {
    admin(&s, &h).await?;
    ok(s.subs.accounts(q.provider).await.map_err(internal)?)
}

#[derive(Deserialize)]
struct ImportReq {
    provider: SubProvider,
    data: Value,
    label: Option<String>,
}

async fn import_account(State(s): State<AppState>, h: HeaderMap, Json(req): Json<ImportReq>) -> Res {
    admin(&s, &h).await?;
    let row = s.subs.import(req.provider, &req.data, req.label).await.map_err(bad)?;
    tracing::info!(account = %row.id, "imported account");
    refresh_catalog_later(&s);
    ok(row)
}

async fn remove_account(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h).await?;
    if !s.subs.remove_account(&id).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"removed": id}))
}

async fn set_account_enabled(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, enabled: bool) -> Res {
    admin(&s, &h).await?;
    if !s.db.set_account_enabled(&id, enabled).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"id": id, "enabled": enabled}))
}

async fn reset_account(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h).await?;
    if !s.db.clear_cooldown(&id).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    s.db.mark_auth_failed(&id, false, None).await.map_err(internal)?;
    ok(json!({"id": id, "reset": true}))
}

#[derive(Deserialize)]
struct PriorityReq {
    priority: i64,
}

async fn set_priority(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, Json(req): Json<PriorityReq>) -> Res {
    admin(&s, &h).await?;
    if !s.db.set_account_priority(&id, req.priority).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"id": id, "priority": req.priority}))
}

async fn refresh_usage(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h).await?;
    ok(s.subs.poll_usage(&id).await.map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?)
}

// ---- login flows

#[derive(Deserialize, Default)]
struct StartReq {
    /// Codex only: use the device-code flow (default) or browser PKCE.
    #[serde(default = "yes")]
    device: bool,
}

fn yes() -> bool {
    true
}

async fn start_login(State(s): State<AppState>, h: HeaderMap, Path(provider): Path<String>, body: Option<Json<StartReq>>) -> Res {
    admin(&s, &h).await?;
    let provider: SubProvider = provider.parse().map_err(bad)?;
    let device = body.map(|b| b.0.device).unwrap_or(true);
    ok(s.subs.start_login(provider, device).await.map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?)
}

async fn flow_status(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h).await?;
    ok(s.subs.flow_status(&id).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "unknown or expired flow"))?)
}

#[derive(Deserialize)]
struct CompleteReq {
    code: String,
}

async fn complete_login(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, Json(req): Json<CompleteReq>) -> Res {
    admin(&s, &h).await?;
    let row = s.subs.complete_login(&id, &req.code).await.map_err(bad)?;
    tracing::info!(account = %row.id, "account logged in");
    refresh_catalog_later(&s);
    ok(row)
}

// ---- providers / usage / reload

#[derive(Deserialize)]
struct ProviderKeyReq {
    api_key: String,
}

/// Gateway providers from the config file, and whether each has a key in the vault.
async fn list_providers(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    let mut out = Vec::new();
    for (name, p) in &s.cfg.gateway.providers {
        let secret = p.secret.clone().unwrap_or_else(|| format!("providers/{name}"));
        let has_key = if p.no_auth {
            false
        } else {
            s.secrets
                .get(&secret)
                .await
                .map_err(internal)?
                .is_some_and(|m| m.get("api_key").is_some_and(|k| k.as_str().is_some_and(|k| !k.is_empty())))
        };
        out.push(json!({
            "name": name,
            "kind": p.kind,
            "enabled": p.is_enabled(),
            "base_url": p.base_url,
            "no_auth": p.no_auth,
            "secret": secret,
            "has_key": has_key,
        }));
    }
    ok(out)
}

async fn set_provider_key(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Json(req): Json<ProviderKeyReq>) -> Res {
    admin(&s, &h).await?;
    if !s.gateway.has_provider(&name) {
        return Err(json_error(StatusCode::NOT_FOUND, "provider is not configured"));
    }
    let data = json!({"api_key": req.api_key}).as_object().cloned().unwrap_or_default();
    let secrets: &dyn SecretStore = &*s.secrets;
    secrets.put(&format!("providers/{name}"), data).await.map_err(internal)?;
    s.gateway.invalidate_keys();
    ok(json!({"provider": name, "stored": true}))
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default = "default_group")]
    group_by: String,
    /// Seconds back from now (default 24h).
    #[serde(default = "default_since")]
    since_secs: i64,
}

fn default_group() -> String {
    "client".into()
}
fn default_since() -> i64 {
    86_400
}

async fn usage(State(s): State<AppState>, h: HeaderMap, Query(q): Query<UsageQuery>) -> Res {
    admin(&s, &h).await?;
    let mut rows = s.db.usage_summary(&q.group_by, now() - q.since_secs).await.map_err(bad)?;
    let _ = &mut rows;
    ok(rows)
}

// ---- MCP gateway

async fn mcp_servers(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    ok(s.mcp.servers().await.map_err(internal)?)
}

/// A server definition from the admin API: the config file's fields plus its
/// name. Unknown fields are refused, as in the config file.
fn server_req(mut body: Value) -> Result<(Option<String>, crate::config::McpServerConfig), Response> {
    let name = match body.as_object_mut().and_then(|m| m.remove("name")) {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) => Some(n.trim().to_string()),
        Some(_) => return Err(json_error(StatusCode::BAD_REQUEST, "name must be a string")),
    };
    let server = serde_json::from_value(body).map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("invalid server: {e}")))?;
    Ok((name, server))
}

fn server_change_error(e: crate::mcp::ServerChangeError) -> Response {
    use crate::mcp::ServerChangeError as E;
    match e {
        E::NotFound(_) => json_error(StatusCode::NOT_FOUND, e.to_string()),
        E::Conflict(_) => json_error(StatusCode::CONFLICT, e.to_string()),
        E::Invalid(_) => json_error(StatusCode::BAD_REQUEST, e.to_string()),
        E::Store(e) => internal(e),
    }
}

async fn mcp_create_server(State(s): State<AppState>, h: HeaderMap, Json(body): Json<Value>) -> Res {
    admin(&s, &h).await?;
    let (name, server) = server_req(body)?;
    let name = name.unwrap_or_default();
    s.mcp.put_server(&name, server, true).await.map_err(server_change_error)?;
    tracing::info!(server = %name, "added MCP server");
    refresh_server_later(&s, &name);
    ok(json!({"server": name, "created": true}))
}

async fn mcp_update_server(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Json(body): Json<Value>) -> Res {
    admin(&s, &h).await?;
    let (body_name, server) = server_req(body)?;
    if body_name.is_some_and(|n| n != name) {
        return Err(json_error(StatusCode::BAD_REQUEST, "server name must match the URL; servers can't be renamed"));
    }
    s.mcp.put_server(&name, server, false).await.map_err(server_change_error)?;
    tracing::info!(server = %name, "changed MCP server");
    refresh_server_later(&s, &name);
    ok(json!({"server": name, "updated": true}))
}

async fn mcp_delete_server(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    s.mcp.delete_server(&name, &s.keys.policies()).await.map_err(server_change_error)?;
    tracing::info!(server = %name, "removed MCP server");
    ok(json!({"server": name, "deleted": true}))
}

/// Read a new or changed server's tools in the background, so saving doesn't
/// wait on the server (or on a login it still needs).
fn refresh_server_later(s: &AppState, name: &str) {
    let (hub, name) = (s.mcp.clone(), name.to_string());
    tokio::spawn(async move {
        if let Err(e) = hub.refresh(&name).await {
            tracing::info!(server = %name, error = %format!("{e:#}"), "listing a saved MCP server's tools failed");
        }
    });
}

async fn mcp_tools(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    ok(s.mcp.server_tools(&name).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such MCP server"))?.as_ref().clone())
}

async fn mcp_refresh(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    if !s.mcp.has_server(&name) {
        return Err(json_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    let tools = s.mcp.refresh(&name).await.map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    ok(json!({"server": name, "tools": tools}))
}

async fn mcp_refresh_all(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    s.mcp.refresh_all().await;
    ok(s.mcp.servers().await.map_err(internal)?)
}

#[derive(Deserialize)]
struct McpSecretReq {
    /// Field -> value; `null` removes the field.
    fields: serde_json::Map<String, Value>,
}

async fn mcp_set_secret(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Json(req): Json<McpSecretReq>) -> Res {
    admin(&s, &h).await?;
    if !s.mcp.has_server(&name) {
        return Err(json_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    let fields: Vec<String> = req.fields.keys().cloned().collect();
    s.mcp.set_secret(&name, req.fields).await.map_err(bad)?;
    tracing::info!(server = %name, ?fields, "updated MCP server secret");
    ok(json!({"server": name, "updated": fields}))
}

async fn mcp_login_start(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    if !s.mcp.has_server(&name) {
        return Err(json_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    ok(s.mcp.login_start(&name).await.map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?)
}

async fn mcp_login_status(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h).await?;
    let (server, status) = s.mcp.flows.status(&id).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "no such login, or it expired"))?;
    let mut v = serde_json::to_value(status).map_err(|e| internal(e.into()))?;
    v["server"] = json!(server);
    ok(v)
}

#[derive(Deserialize)]
struct McpLoginComplete {
    /// The URL the browser ended on after consent.
    redirect_url: String,
}

async fn mcp_login_complete(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, Json(req): Json<McpLoginComplete>) -> Res {
    admin(&s, &h).await?;
    let server = s.mcp.login_complete(&id, &req.redirect_url).await.map_err(bad)?;
    ok(json!({"server": server, "status": "complete"}))
}

#[derive(Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
}

/// The browser lands here after consent when `server.public_url` is set. The
/// unguessable `state` of a login an admin started is what authorizes it.
async fn mcp_login_callback(State(s): State<AppState>, uri: axum::http::Uri, Query(q): Query<CallbackQuery>) -> Response {
    let page = |status: StatusCode, title: &str, text: &str| {
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title>\
             <body style=\"font-family:sans-serif;max-width:36rem;margin:4rem auto\"><h1>{title}</h1><p>{text}</p>"
        );
        (
            status,
            [(axum::http::header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'")],
            axum::response::Html(html),
        )
            .into_response()
    };
    let Some(id) = q.state.as_deref().and_then(|st| s.mcp.flows.by_state(st)) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Unknown login",
            "This sign-in link doesn't belong to a login in progress. Start again from xolotl.",
        );
    };
    if q.code.is_none() {
        return page(StatusCode::BAD_REQUEST, "Sign-in refused", "The authorization server did not grant access. Start again from xolotl.");
    }
    // The redirect URL as the authorization server built it, so `iss` and `state` are checked as sent.
    let url = format!("http://callback{}", uri.path_and_query().map(|p| p.as_str()).unwrap_or(""));
    match s.mcp.login_complete(&id, &url).await {
        Ok(server) => page(StatusCode::OK, "Signed in", &format!("xolotl is signed in to {server}. You can close this tab.")),
        Err(e) => page(StatusCode::BAD_REQUEST, "Sign-in failed", &html_escape(&format!("{e:#}"))),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn mcp_client_metadata(State(s): State<AppState>) -> Response {
    match s.mcp.client_metadata() {
        Some(doc) => Json(doc).into_response(),
        None => {
            json_error(StatusCode::NOT_FOUND, "no client metadata document: set server.public_url (https) or mcp.oauth.client_metadata_url")
        }
    }
}

async fn mcp_logout(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> Res {
    admin(&s, &h).await?;
    if !s.mcp.has_server(&name) {
        return Err(json_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    s.mcp.logout(&name).await.map_err(bad)?;
    tracing::info!(server = %name, "signed out of MCP server");
    ok(json!({"server": name, "signed_out": true}))
}

#[derive(Deserialize)]
struct McpUsageQuery {
    #[serde(default = "default_mcp_group")]
    group_by: String,
    #[serde(default = "default_since")]
    since_secs: i64,
}

fn default_mcp_group() -> String {
    "server".into()
}

async fn mcp_usage(State(s): State<AppState>, h: HeaderMap, Query(q): Query<McpUsageQuery>) -> Res {
    admin(&s, &h).await?;
    ok(s.db.mcp_usage_summary(&q.group_by, now() - q.since_secs).await.map_err(bad)?)
}

#[derive(Deserialize)]
struct McpCallsQuery {
    #[serde(default = "default_calls_limit")]
    limit: i64,
}

fn default_calls_limit() -> i64 {
    50
}

async fn mcp_calls(State(s): State<AppState>, h: HeaderMap, Query(q): Query<McpCallsQuery>) -> Res {
    admin(&s, &h).await?;
    ok(s.db.recent_mcp_calls(q.limit.clamp(1, 500)).await.map_err(internal)?)
}

/// Health and settings for the web UI's overview.
async fn status(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    let database = match s.db.ping().await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    };
    let vault = match &s.vault {
        Some(v) => match v.health().await {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
        },
        None => json!({"ok": true, "note": "in-memory store"}),
    };
    let sub = &s.cfg.subscription;
    ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "database": database,
        "vault": vault,
        "catalog_refreshed_secs_ago": s.catalog.last_refresh().map(|t| t.elapsed().as_secs()),
        "unsaved_credentials": s.subs.unsaved_count(),
        "recent_errors": s.db.recent_errors(20).await.map_err(internal)?,
        "dashboard_url": s.cfg.telemetry.dashboard_url,
        // To show clients the full vault path of their key.
        "vault_mount": s.cfg.vault.mount,
        "vault_prefix": s.cfg.vault.prefix,
        "subscription": {
            "strategy": sub.strategy,
            "soft_limit_percent": sub.soft_limit_percent,
            "session_affinity_secs": sub.session_affinity_secs,
            "usage_poll_secs": sub.usage_poll_secs,
        },
    }))
}

async fn reload(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h).await?;
    let (policies, keys) = s.keys.reload().await.map_err(internal)?;
    let accounts = s.subs.sync().await.map_err(internal)?;
    s.gateway.invalidate_keys();
    refresh_catalog_later(&s);
    ok(json!({"policies": policies, "keys": keys, "accounts": accounts}))
}
