//! CLIProxyAPI-compatible management endpoints used by T3Code hubs:
//! `GET auth-files`, `POST api-call`, `POST reset-quota`. T3Code sends
//! `Authorization: Bearer <management key>` and requires 2xx JSON responses.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, authorize};
use crate::db::now;
use crate::keys::Scope;

pub fn router() -> Router<AppState> {
    Router::new().route("/auth-files", get(auth_files)).route("/api-call", post(api_call)).route("/reset-quota", post(reset_quota))
}

/// CLIProxyAPI's management errors are `{"error": "..."}`.
fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"error": msg.into()}))).into_response()
}

async fn check(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    authorize(state, headers, Scope::Management).await.map(|_| ()).map_err(|_| err(StatusCode::UNAUTHORIZED, "invalid management key"))
}

async fn auth_files(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = check(&state, &headers).await {
        return r;
    }
    let accounts = match state.subs.accounts(None).await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let t = now();
    let mut files: Vec<Value> = accounts
        .into_iter()
        .map(|a| {
            let r = &a.row;
            let status = if !r.enabled {
                "disabled"
            } else if r.auth_failed {
                "error"
            } else {
                "active"
            };
            let mut e = Map::new();
            e.insert("id".into(), json!(r.id));
            e.insert("auth_index".into(), json!(a.auth_index));
            e.insert("name".into(), json!(r.id));
            e.insert("type".into(), json!(r.provider));
            e.insert("provider".into(), json!(r.provider));
            e.insert("label".into(), json!(r.label));
            e.insert("status".into(), json!(status));
            e.insert("status_message".into(), json!(r.last_error.clone().unwrap_or_default()));
            e.insert("disabled".into(), json!(!r.enabled));
            e.insert("unavailable".into(), json!(!a.available));
            e.insert("source".into(), json!("vault"));
            e.insert("priority".into(), json!(r.priority));
            // Optional fields are omitted rather than null: T3 rejects nulls.
            if let Some(email) = &r.email {
                e.insert("email".into(), json!(email));
            }
            if let Some(until) = r.cooldown_until.filter(|u| *u > t)
                && let Some(ts) = chrono::DateTime::from_timestamp(until, 0)
            {
                e.insert("next_retry_after".into(), json!(ts.to_rfc3339()));
            }
            if r.provider == "codex" {
                let mut tok = Map::new();
                if let Some(id) = &r.external_id {
                    tok.insert("chatgpt_account_id".into(), json!(id));
                }
                if let Some(plan) = &r.plan {
                    tok.insert("plan_type".into(), json!(plan));
                    tok.insert("chatgpt_plan_type".into(), json!(plan));
                }
                e.insert("id_token".into(), Value::Object(tok));
            }
            Value::Object(e)
        })
        .collect();
    files.sort_by_key(|f| f["name"].as_str().unwrap_or("").to_lowercase());
    let observed = chrono::Utc::now().to_rfc3339();
    Json(json!({"observed_at": observed, "files": files})).into_response()
}

#[derive(Deserialize)]
struct ApiCallRequest {
    #[serde(alias = "authIndex", alias = "AuthIndex", default)]
    auth_index: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    header: HashMap<String, String>,
    #[serde(default)]
    data: Option<String>,
}

async fn api_call(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = check(&state, &headers).await {
        return r;
    }
    let Ok(req) = serde_json::from_slice::<ApiCallRequest>(&body) else {
        return err(StatusCode::BAD_REQUEST, "invalid body");
    };
    if req.method.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "missing method");
    }
    if req.url.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "missing url");
    }
    let account = match state.subs.find_by_auth_index(req.auth_index.trim()).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(StatusCode::BAD_REQUEST, "auth credential not found for auth_index"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match state.subs.api_call(&account, &req.method, &req.url, &req.header, req.data.as_deref()).await {
        Ok((status, h, body)) => {
            let mut header: Map<String, Value> = Map::new();
            for (k, v) in &h {
                let entry = header.entry(canonical(k.as_str())).or_insert_with(|| json!([]));
                if let (Some(arr), Ok(s)) = (entry.as_array_mut(), v.to_str()) {
                    arr.push(json!(s));
                }
            }
            Json(json!({"status_code": status, "header": header, "body": body})).into_response()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let status = if msg.contains("not allowed") || msg.contains("invalid") {
                StatusCode::BAD_REQUEST
            } else if msg.contains("log in") || msg.contains("refresh") {
                return err(StatusCode::BAD_REQUEST, "auth token refresh failed");
            } else {
                StatusCode::BAD_GATEWAY
            };
            err(status, msg)
        }
    }
}

/// Go's canonical header casing (`content-type` -> `Content-Type`).
fn canonical(k: &str) -> String {
    k.split('-')
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Deserialize)]
struct ResetQuota {
    #[serde(default)]
    auth_index: String,
}

async fn reset_quota(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = check(&state, &headers).await {
        return r;
    }
    let Ok(req) = serde_json::from_slice::<ResetQuota>(&body) else {
        return err(StatusCode::BAD_REQUEST, "invalid request body");
    };
    if req.auth_index.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "auth_index is required");
    }
    let account = match state.subs.find_by_auth_index(req.auth_index.trim()).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(StatusCode::NOT_FOUND, "auth not found"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if let Err(e) = state.db.clear_cooldown(&account.id).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("failed to reset quota: {e:#}"));
    }
    tracing::info!(account = %account.id, "quota cooldown reset via management API");
    // Refresh the stored usage so selection sees the reset immediately.
    let subs = state.subs.clone();
    let id = account.id.clone();
    tokio::spawn(async move {
        let _ = subs.poll_usage(&id).await;
    });
    Json(json!({"status": "ok", "auth_index": req.auth_index, "models": []})).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn canonical_headers() {
        assert_eq!(super::canonical("content-type"), "Content-Type");
        assert_eq!(super::canonical("x-codex-primary-used-percent"), "X-Codex-Primary-Used-Percent");
    }
}
