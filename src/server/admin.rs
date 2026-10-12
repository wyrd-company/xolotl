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
use crate::secrets::SecretStore;
use crate::subscription::SubProvider;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/keys", get(list_keys).post(issue_key))
        .route("/keys/{id}", delete(revoke_key))
        .route("/keys/{id}/rotate", post(rotate_key))
        .route("/keys/{id}/enable", post(|s, h, p| set_key_disabled(s, h, p, false)))
        .route("/keys/{id}/disable", post(|s, h, p| set_key_disabled(s, h, p, true)))
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
        .route("/providers/{name}/key", post(set_provider_key))
        .route("/usage", get(usage))
        .route("/reload", post(reload))
}

type Res = Result<Response, Response>;

fn admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    authorize(state, headers, Scope::Admin).map(|_| ())
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

// ---- keys

async fn list_keys(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h)?;
    ok(s.keys.list().iter().map(|k| k.redacted()).collect::<Vec<_>>())
}

async fn issue_key(State(s): State<AppState>, h: HeaderMap, Json(req): Json<IssueRequest>) -> Res {
    admin(&s, &h)?;
    let issued = s.keys.issue(req).await.map_err(bad)?;
    tracing::info!(id = %issued.record["id"], "issued client key");
    ok(issued)
}

#[derive(Deserialize, Default)]
struct RotateReq {
    #[serde(default)]
    grace_secs: i64,
}

async fn rotate_key(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, body: Option<Json<RotateReq>>) -> Res {
    admin(&s, &h)?;
    let grace = body.map(|b| b.0.grace_secs).unwrap_or(0);
    let issued = s.keys.rotate(&id, grace).await.map_err(|e| json_error(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    tracing::info!(%id, grace, "rotated client key");
    ok(issued)
}

async fn revoke_key(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h)?;
    if !s.keys.revoke(&id).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such key"));
    }
    tracing::info!(%id, "revoked client key");
    ok(json!({"revoked": id}))
}

async fn set_key_disabled(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, disabled: bool) -> Res {
    admin(&s, &h)?;
    ok(s.keys.set_disabled(&id, disabled).await.map_err(|e| json_error(StatusCode::NOT_FOUND, format!("{e:#}")))?)
}

// ---- accounts

#[derive(Deserialize)]
struct ProviderFilter {
    provider: Option<SubProvider>,
}

async fn list_accounts(State(s): State<AppState>, h: HeaderMap, Query(q): Query<ProviderFilter>) -> Res {
    admin(&s, &h)?;
    ok(s.subs.accounts(q.provider).await.map_err(internal)?)
}

#[derive(Deserialize)]
struct ImportReq {
    provider: SubProvider,
    data: Value,
    label: Option<String>,
}

async fn import_account(State(s): State<AppState>, h: HeaderMap, Json(req): Json<ImportReq>) -> Res {
    admin(&s, &h)?;
    let row = s.subs.import(req.provider, &req.data, req.label).await.map_err(bad)?;
    tracing::info!(account = %row.id, "imported account");
    ok(row)
}

async fn remove_account(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h)?;
    if !s.subs.remove_account(&id).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"removed": id}))
}

async fn set_account_enabled(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, enabled: bool) -> Res {
    admin(&s, &h)?;
    if !s.db.set_account_enabled(&id, enabled).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"id": id, "enabled": enabled}))
}

async fn reset_account(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h)?;
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
    admin(&s, &h)?;
    if !s.db.set_account_priority(&id, req.priority).await.map_err(internal)? {
        return Err(json_error(StatusCode::NOT_FOUND, "no such account"));
    }
    ok(json!({"id": id, "priority": req.priority}))
}

async fn refresh_usage(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h)?;
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
    admin(&s, &h)?;
    let provider: SubProvider = provider.parse().map_err(bad)?;
    let device = body.map(|b| b.0.device).unwrap_or(true);
    ok(s.subs.start_login(provider, device).await.map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("{e:#}")))?)
}

async fn flow_status(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Res {
    admin(&s, &h)?;
    ok(s.subs.flow_status(&id).ok_or_else(|| json_error(StatusCode::NOT_FOUND, "unknown or expired flow"))?)
}

#[derive(Deserialize)]
struct CompleteReq {
    code: String,
}

async fn complete_login(State(s): State<AppState>, h: HeaderMap, Path(id): Path<String>, Json(req): Json<CompleteReq>) -> Res {
    admin(&s, &h)?;
    let row = s.subs.complete_login(&id, &req.code).await.map_err(bad)?;
    tracing::info!(account = %row.id, "account logged in");
    ok(row)
}

// ---- providers / usage / reload

#[derive(Deserialize)]
struct ProviderKeyReq {
    api_key: String,
}

async fn set_provider_key(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Json(req): Json<ProviderKeyReq>) -> Res {
    admin(&s, &h)?;
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
    admin(&s, &h)?;
    let mut rows = s.db.usage_summary(&q.group_by, now() - q.since_secs).await.map_err(bad)?;
    if q.group_by == "client" {
        // Show key names next to ids.
        for r in &mut rows {
            if let Some(k) = r.key.as_deref().and_then(|id| s.keys.get(id)) {
                r.key = Some(format!("{} ({})", k.id, k.name));
            }
        }
    }
    ok(rows)
}

async fn reload(State(s): State<AppState>, h: HeaderMap) -> Res {
    admin(&s, &h)?;
    let keys = s.keys.reload().await.map_err(internal)?;
    let accounts = s.subs.sync().await.map_err(internal)?;
    s.gateway.invalidate_keys();
    ok(json!({"keys": keys, "accounts": accounts}))
}
