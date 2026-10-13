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
    let saved = s.keys.put_policy(policy, &s.catalog.known_providers()).await.map_err(bad)?;
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
    policy.validate(&s.catalog.known_providers()).map_err(bad)?;
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
    json!({"policy": policy.name, "models": models, "shared": policy.shared_names(&catalog)})
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
