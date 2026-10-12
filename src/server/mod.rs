//! HTTP server: routing, client authentication, and the LLM entry points.

// Handlers return ready-made error responses; boxing them buys nothing.
#![allow(clippy::result_large_err)]

mod admin;
mod management;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::db::Db;
use crate::gateway::Gateway;
use crate::keys::{KeyManager, Principal, Scope};
use crate::proxy::{self, forward_request_headers};
use crate::secrets::Secrets;
use crate::subscription::{SubProvider, Subscriptions};
use crate::vault::VaultClient;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub secrets: Secrets,
    pub keys: Arc<KeyManager>,
    pub subs: Arc<Subscriptions>,
    pub gateway: Arc<Gateway>,
    pub vault: Option<VaultClient>,
}

pub fn router(state: AppState) -> Router {
    let max_body = state.cfg.server.max_body_bytes;
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/models", get(models))
        .route("/v1/messages", post(|s, r| llm(s, r, "/v1/messages")))
        .route("/v1/messages/count_tokens", post(|s, r| llm(s, r, "/v1/messages/count_tokens")))
        .route("/v1/responses", post(|s, r| llm(s, r, "/v1/responses")).get(websocket_unsupported))
        .route("/v1/responses/compact", post(|s, r| llm(s, r, "/v1/responses/compact")))
        // chatgpt_base_url-style aliases used by some Codex setups.
        .route("/backend-api/codex/responses", post(|s, r| llm(s, r, "/v1/responses")).get(websocket_unsupported))
        .route("/backend-api/codex/responses/compact", post(|s, r| llm(s, r, "/v1/responses/compact")))
        .route("/v1/{*rest}", post(unified))
        .nest("/v0/management", management::router())
        .nest("/admin", admin::router())
        .route("/{provider}/{*rest}", any(passthrough))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request| tracing::info_span!("http", method = %req.method(), path = %req.uri().path())),
        )
        .with_state(state)
}

pub async fn serve(state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(state.cfg.server.listen).await?;
    tracing::info!(addr = %state.cfg.server.listen, "xolotl listening");
    axum::serve(listener, router(state)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

/// Background maintenance: reload keys and accounts, poll usage, prune logs.
pub fn spawn_background(state: AppState) {
    let s = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = s.keys.reload().await {
                tracing::warn!(error = %e, "client key reload failed");
            }
            if let Err(e) = s.subs.sync().await {
                tracing::warn!(error = %e, "account sync failed");
            }
        }
    });
    let poll = state.cfg.subscription.usage_poll_secs;
    if poll > 0 {
        let s = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(poll));
            loop {
                tick.tick().await;
                s.subs.poll_all().await;
            }
        });
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(86_400));
        loop {
            tick.tick().await;
            match state.db.prune(state.cfg.database.retention_days).await {
                Ok(n) if n > 0 => tracing::info!(rows = n, "pruned request log"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "request log prune failed"),
            }
        }
    });
}

// ------------------------------------------------------------------ auth

/// The presented credential: `Authorization: Bearer`, a raw `Authorization`
/// value, `x-api-key` (Anthropic clients) or `x-management-key`.
pub fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        let token = match v.split_once(' ') {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
            _ => v,
        };
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    ["x-api-key", "x-management-key"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

pub fn authorize(state: &AppState, headers: &HeaderMap, scope: Scope) -> Result<Principal, Response> {
    let Some(key) = presented_key(headers) else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "missing API key"));
    };
    match state.keys.authenticate(&key) {
        Some(p) if p.has_scope(scope) => Ok(p),
        Some(_) => Err(json_error(StatusCode::FORBIDDEN, "key lacks the required scope")),
        None => Err(json_error(StatusCode::UNAUTHORIZED, "invalid API key")),
    }
}

pub fn json_error(status: StatusCode, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    (status, Json(json!({"error": {"type": status_kind(status), "message": msg}}))).into_response()
}

fn status_kind(s: StatusCode) -> &'static str {
    match s.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    }
}

// --------------------------------------------------------------- handlers

async fn readyz(State(state): State<AppState>) -> Response {
    if let Err(e) = state.db.ping().await {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, format!("database: {e}"));
    }
    if let Some(v) = &state.vault
        && let Err(e) = v.health().await
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, format!("vault: {e:#}"));
    }
    (StatusCode::OK, "ready").into_response()
}

/// Codex's built-in provider tries a WebSocket first; 426 makes it fall back
/// to HTTP cleanly.
async fn websocket_unsupported() -> Response {
    json_error(StatusCode::UPGRADE_REQUIRED, "websocket transport is not supported; use HTTP")
}

fn pool_allows(p: &Principal, pool: SubProvider, model: &str) -> bool {
    p.allows_model(model) || p.allows_model(&format!("{}/{}", pool.as_str(), model.trim_start_matches(&format!("{}/", pool.as_str()))))
}

/// Route a JSON request by its `model`:
/// - an alias or `<gateway-provider>/<model>` goes to the gateway,
/// - `opencode-go/<model>` goes to the OpenCode Go account pool,
/// - otherwise `/v1/messages*` goes to the Claude pool and `/v1/responses*`
///   to the Codex pool.
async fn route_json(state: &AppState, principal: Principal, path: &str, headers: HeaderMap, body: Bytes) -> Response {
    let anthropic = path.starts_with("/v1/messages");
    let err = |status: StatusCode, kind: &str, msg: String| {
        if anthropic { proxy::anthropic_error(status, kind, msg) } else { proxy::error_response(status, kind, msg) }
    };
    let json: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return err(StatusCode::BAD_REQUEST, "invalid_request_error", "request body must be a JSON object".into()),
    };
    let Some(model) = json.get("model").and_then(Value::as_str).map(str::to_string) else {
        return err(StatusCode::BAD_REQUEST, "invalid_request_error", "missing `model`".into());
    };
    let fwd = forward_request_headers(&headers);
    let client_id = principal.client_id().map(str::to_string);

    if let Some(targets) = state.gateway.resolve(&model) {
        if !principal.allows_model(&model) {
            return err(StatusCode::FORBIDDEN, "permission_error", format!("model {model} is not allowed for this key"));
        }
        return state.gateway.execute(&targets, Method::POST, path, fwd, json, client_id.as_deref(), &model).await;
    }
    let pool = if model.starts_with("opencode-go/") {
        SubProvider::OpencodeGo
    } else if anthropic {
        SubProvider::Claude
    } else if path.starts_with("/v1/responses") {
        SubProvider::Codex
    } else {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("unknown model {model:?}: use <provider>/<model> with a configured provider, or an alias"),
        );
    };
    if !pool_allows(&principal, pool, &model) {
        return err(StatusCode::FORBIDDEN, "permission_error", format!("model {model} is not allowed for this key"));
    }
    state.subs.execute(pool, Method::POST, path, fwd, body, client_id.as_deref(), Some(&model)).await
}

async fn llm(State(state): State<AppState>, req: Request, path: &'static str) -> Response {
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let body = match axum::body::to_bytes(body, state.cfg.server.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    route_json(&state, principal, path, parts.headers, body).await
}

/// Any other `POST /v1/...` JSON endpoint (chat/completions, embeddings, ocr...).
async fn unified(State(state): State<AppState>, Path(rest): Path<String>, req: Request) -> Response {
    let path = format!("/v1/{rest}");
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let body = match axum::body::to_bytes(body, state.cfg.server.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    route_json(&state, principal, &path, parts.headers, body).await
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match authorize(&state, &headers, Scope::Proxy) {
        Ok(p) => p,
        Err(r) => return r,
    };
    // Claude Code asks with anthropic-version; serve the Claude account's list.
    if headers.contains_key("anthropic-version") {
        let fwd = forward_request_headers(&headers);
        return state.subs.execute(SubProvider::Claude, Method::GET, "/v1/models", fwd, Bytes::new(), principal.client_id(), None).await;
    }
    let data: Vec<Value> = state
        .gateway
        .list_models()
        .await
        .into_iter()
        .filter(|m| m.get("id").and_then(Value::as_str).is_some_and(|id| principal.allows_model(id)))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// `/<provider>/<rest>`: raw passthrough to a gateway provider.
async fn passthrough(State(state): State<AppState>, Path((provider, rest)): Path<(String, String)>, req: Request) -> Response {
    if !state.gateway.has_provider(&provider) {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut path = format!("/{rest}");
    if let Some(q) = parts.uri.query() {
        path.push('?');
        path.push_str(q);
    }
    let fwd = forward_request_headers(&parts.headers);
    let is_json =
        parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("application/json"));

    if is_json {
        let bytes = match axum::body::to_bytes(body, state.cfg.server.max_body_bytes).await {
            Ok(b) => b,
            Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        };
        let model = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.get("model").and_then(Value::as_str).map(|m| format!("{provider}/{m}")));
        let allowed = match &model {
            Some(m) => principal.allows_model(m),
            None => principal.allows_model(&format!("{provider}/*")),
        };
        if !allowed {
            return json_error(StatusCode::FORBIDDEN, "model is not allowed for this key");
        }
        return state
            .gateway
            .passthrough(&provider, parts.method, &path, fwd, reqwest::Body::from(bytes), principal.client_id(), model.as_deref())
            .await;
    }
    if !principal.allows_model(&format!("{provider}/*")) {
        return json_error(StatusCode::FORBIDDEN, "this key may not use the raw provider API");
    }
    let stream = Body::new(body).into_data_stream();
    state.gateway.passthrough(&provider, parts.method, &path, fwd, reqwest::Body::wrap_stream(stream), principal.client_id(), None).await
}

#[cfg(test)]
mod tests;
