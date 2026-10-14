//! HTTP server: routing, client authentication, and the LLM entry points.

// Handlers return ready-made error responses; boxing them buys nothing.
#![allow(clippy::result_large_err)]

mod admin;
mod management;
pub mod session;
mod ui;

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

use crate::catalog::{ModelCatalog, Route, Wire};
use crate::config::Config;
use crate::db::Db;
use crate::gateway::Gateway;
use crate::keys::{KeyManager, Principal, Scope};
use crate::policy::{Target, View};
use crate::proxy::{self, forward_request_headers};
use crate::secrets::Secrets;
use crate::subscription::{SubProvider, Subscriptions};
use crate::translate;
use crate::vault::VaultClient;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub secrets: Secrets,
    pub keys: Arc<KeyManager>,
    pub subs: Arc<Subscriptions>,
    pub gateway: Arc<Gateway>,
    pub catalog: Arc<ModelCatalog>,
    pub vault: Option<VaultClient>,
    pub sessions: Arc<session::Sessions>,
}

impl AppState {
    /// The model names a principal's policy exposes right now.
    pub fn view(&self, p: &Principal) -> View {
        p.policy.resolve(&self.catalog.models())
    }
}

pub fn router(state: AppState) -> Router {
    let max_body = state.cfg.server.max_body_bytes;
    Router::new()
        .route("/", get(ui::index))
        .route("/assets/{*path}", get(ui::asset))
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
        .nest("/admin", admin::router().merge(session::router()))
        .route("/opencode-go/{*rest}", any(opencode_go_passthrough))
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

/// Background maintenance: reload keys and accounts, refresh the model
/// catalog, poll usage, prune logs.
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
    let s = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(s.cfg.catalog.refresh_secs.max(60)));
        loop {
            tick.tick().await;
            s.catalog.refresh(&s.subs, &s.gateway).await;
        }
    });
    // Retry vault writes of refreshed credentials that failed earlier.
    let s = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            if s.subs.unsaved_count() > 0 {
                s.subs.flush_unsaved().await;
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

/// Resolve the caller from its key. Without a key, an admin route also
/// accepts a web UI session (cookie plus CSRF header).
pub async fn authorize(state: &AppState, headers: &HeaderMap, scope: Scope) -> Result<Principal, Response> {
    let Some(key) = presented_key(headers) else {
        if scope == Scope::Admin
            && let Some(p) = state.sessions.principal(headers)
        {
            return Ok(p);
        }
        return Err(json_error(StatusCode::UNAUTHORIZED, "missing API key"));
    };
    match state.keys.authenticate(&key).await {
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

fn pool(provider: &str) -> Option<SubProvider> {
    provider.parse().ok()
}

/// Route a JSON request by its `model` through the caller's policy view,
/// trying an alias's targets in order (falling back on 429 / 5xx). A target
/// whose provider doesn't speak the endpoint's format is reached through
/// translation.
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
    let known = state.catalog.known_providers();
    let Some(targets) = state.view(&principal).lookup(&principal.policy, &known, &model) else {
        return err(StatusCode::NOT_FOUND, "not_found_error", format!("model {model:?} is not available to this key"));
    };

    let wire = Wire::for_path(path);
    let route = |t: &Target| wire.and_then(|w| state.catalog.route(&t.provider, &t.model, w, path));
    let (usable, unusable): (Vec<Target>, Vec<Target>) = targets.into_iter().partition(|t| match wire {
        Some(_) => route(t).is_some(),
        // Pools only serve the LLM endpoints.
        None => pool(&t.provider).is_none(),
    });
    if usable.is_empty() {
        let hint = unusable
            .first()
            .map(|t| {
                let paths: Vec<&str> = state.catalog.wires(&t.provider, &t.model).iter().map(|w| w.path()).collect();
                format!(" ({} is served via {})", t.qualified(), paths.join(", "))
            })
            .unwrap_or_default();
        return err(StatusCode::BAD_REQUEST, "invalid_request_error", format!("model {model:?} is not available on {path}{hint}"));
    }

    let fwd = forward_request_headers(&headers);
    let client_id = principal.client_id().map(str::to_string);
    let last = usable.len() - 1;
    for (i, target) in usable.iter().enumerate() {
        let (resp, translation) = match (wire, route(target)) {
            (Some(client), Some(Route::Translated(upstream))) => {
                let (body, t) = match translate::request(client, upstream, &json, &target.model) {
                    Ok(x) => x,
                    Err(msg) => return err(StatusCode::BAD_REQUEST, "invalid_request_error", msg),
                };
                let resp =
                    send(state, target, upstream.path(), translated_headers(&fwd, upstream), body, client_id.as_deref(), &model).await;
                (resp, Some(t))
            }
            _ => {
                let resp = send(state, target, path, fwd.clone(), json.clone(), client_id.as_deref(), &model).await;
                let streams = json.get("stream").and_then(Value::as_bool).unwrap_or(false);
                let grok = pool(&target.provider) == Some(SubProvider::Grok);
                let resp = if grok && wire == Some(Wire::Responses) && !streams { collect_grok(resp).await } else { resp };
                (resp, None)
            }
        };
        let status = resp.status().as_u16();
        if i < last && (status == 429 || status >= 500) {
            tracing::info!(target = %target.qualified(), status, "falling back to next alias target");
            continue;
        }
        return match translation {
            Some(t) => t.response(resp).await,
            None => resp,
        };
    }
    unreachable!("loop returns on the last target")
}

/// Grok's proxy only streams: answer a Responses client that didn't ask to
/// stream with the collected JSON body. Translated requests are collected
/// by the translation instead.
async fn collect_grok(resp: Response) -> Response {
    let sse = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("text/event-stream"));
    if !resp.status().is_success() || !sse {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => return proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", format!("reading upstream stream: {e}")),
    };
    match crate::subscription::grok::collect_response(&bytes) {
        Ok(v) => {
            parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(v.to_string()))
        }
        Err(msg) => proxy::error_response(StatusCode::BAD_GATEWAY, "api_error", msg),
    }
}

/// Send a JSON request to one target: a subscription pool or a gateway provider.
async fn send(
    state: &AppState,
    target: &Target,
    path: &str,
    headers: HeaderMap,
    mut body: Value,
    client_id: Option<&str>,
    model: &str,
) -> Response {
    match pool(&target.provider) {
        Some(p) => {
            body["model"] = Value::String(target.model.clone());
            let bytes = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
            state.subs.execute(p, Method::POST, path, headers, bytes, client_id, Some(model)).await
        }
        None => state.gateway.execute(target, Method::POST, path, headers, body, client_id, model).await,
    }
}

/// Client headers for a translated request: those of the client's own format
/// and content negotiation don't apply upstream.
fn translated_headers(fwd: &HeaderMap, upstream: Wire) -> HeaderMap {
    let mut h = HeaderMap::with_capacity(fwd.len());
    for (k, v) in fwd {
        let name = k.as_str();
        if name == "accept" || (upstream != Wire::Messages && name.starts_with("anthropic-")) {
            continue;
        }
        h.append(k.clone(), v.clone());
    }
    if upstream == Wire::Messages && !h.contains_key("anthropic-version") {
        h.insert("anthropic-version", header::HeaderValue::from_static("2023-06-01"));
    }
    h
}

async fn llm(State(state): State<AppState>, req: Request, path: &'static str) -> Response {
    llm_dynamic(state, req, path).await
}

/// Any other `POST /v1/...` JSON endpoint (chat/completions, embeddings, ocr...).
async fn unified(State(state): State<AppState>, Path(rest): Path<String>, req: Request) -> Response {
    let path = format!("/v1/{rest}");
    llm_dynamic(state, req, &path).await
}

async fn llm_dynamic(state: AppState, req: Request, path: &str) -> Response {
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let body = match axum::body::to_bytes(body, state.cfg.server.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    route_json(&state, principal, path, parts.headers, body).await
}

/// The caller's model list: in the Grok CLI's shape for `grok-shell` clients,
/// in Anthropic's shape when the request carries `anthropic-version` (Claude
/// Code). Each entry names the endpoints its first target is served on
/// natively and through translation.
async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match authorize(&state, &headers, Scope::Proxy).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let view = state.view(&principal);
    let endpoints = |ts: &Vec<Target>| {
        let (native, translated) = ts.first().map(|t| state.catalog.endpoints(&t.provider, &t.model)).unwrap_or_default();
        (json!(native), json!(translated))
    };
    // The Grok CLI reads its own shape and speaks only Responses: list the
    // names some target serves there, natively or through translation.
    if crate::subscription::grok::is_grok_shell(&headers) {
        let responses = |ts: &Vec<Target>| {
            ts.iter().any(|t| state.catalog.route(&t.provider, &t.model, Wire::Responses, Wire::Responses.path()).is_some())
        };
        let data: Vec<Value> = view
            .names
            .iter()
            .filter(|(_, ts)| responses(ts))
            .map(|(name, _)| json!({"id": name, "model": name, "name": name, "api_backend": "responses", "supported_in_api": true}))
            .collect();
        return Json(json!({"object": "list", "data": data})).into_response();
    }
    if headers.contains_key("anthropic-version") {
        let data: Vec<Value> = view
            .names
            .iter()
            .map(|(name, ts)| {
                let (native, translated) = endpoints(ts);
                json!({
                    "id": name, "type": "model", "display_name": name, "created_at": "1970-01-01T00:00:00Z",
                    "endpoints": native, "translated_endpoints": translated,
                })
            })
            .collect();
        let first = data.first().map(|m| m["id"].clone()).unwrap_or(Value::Null);
        let last = data.last().map(|m| m["id"].clone()).unwrap_or(Value::Null);
        return Json(json!({"data": data, "has_more": false, "first_id": first, "last_id": last})).into_response();
    }
    let data: Vec<Value> = view
        .names
        .iter()
        .map(|(name, ts)| {
            let (native, translated) = endpoints(ts);
            json!({
                "id": name, "object": "model", "owned_by": ts.first().map(|t| t.provider.as_str()).unwrap_or(""),
                "endpoints": native, "translated_endpoints": translated,
            })
        })
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// Read a passthrough body: JSON bodies are buffered so their `model` can be
/// checked against the policy; anything else streams.
async fn passthrough_body(
    state: &AppState,
    principal: &Principal,
    provider: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<(Option<Bytes>, Option<String>, Option<Body>), Response> {
    let is_json = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("application/json"));
    if !principal.policy.allows_provider(provider) {
        return Err(json_error(StatusCode::FORBIDDEN, format!("this key may not use {provider}")));
    }
    if !is_json {
        return Ok((None, None, Some(body)));
    }
    let bytes = axum::body::to_bytes(body, state.cfg.server.max_body_bytes)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))?;
    let model = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string));
    if let Some(m) = &model
        && !principal.policy.allows_model(provider, m)
    {
        return Err(json_error(StatusCode::FORBIDDEN, format!("model {m} is not allowed for this key")));
    }
    Ok((Some(bytes), model, None))
}

fn path_and_query(rest: &str, uri: &axum::http::Uri) -> String {
    let mut path = format!("/{rest}");
    if let Some(q) = uri.query() {
        path.push('?');
        path.push_str(q);
    }
    path
}

/// `/<provider>/<rest>`: raw passthrough to a gateway provider.
async fn passthrough(State(state): State<AppState>, Path((provider, rest)): Path<(String, String)>, req: Request) -> Response {
    if !state.gateway.has_provider(&provider) {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (bytes, model, stream) = match passthrough_body(&state, &principal, &provider, &parts.headers, body).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let path = path_and_query(&rest, &parts.uri);
    let fwd = forward_request_headers(&parts.headers);
    let label = model.map(|m| format!("{provider}/{m}"));
    let upstream = match (bytes, stream) {
        (Some(b), _) => reqwest::Body::from(b),
        (None, Some(s)) => reqwest::Body::wrap_stream(s.into_data_stream()),
        (None, None) => reqwest::Body::from(Vec::new()),
    };
    state.gateway.passthrough(&provider, parts.method, &path, fwd, upstream, principal.client_id(), label.as_deref()).await
}

/// `/opencode-go/<rest>`: raw OpenCode Go API through the account pool
/// (load balancing, failover and usage tracking still apply).
async fn opencode_go_passthrough(State(state): State<AppState>, Path(rest): Path<String>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let principal = match authorize(&state, &parts.headers, Scope::Proxy).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let provider = SubProvider::OpencodeGo.as_str();
    let (bytes, model, stream) = match passthrough_body(&state, &principal, provider, &parts.headers, body).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Failover may resend the body, so it is always buffered here.
    let bytes = match (bytes, stream) {
        (Some(b), _) => b,
        (None, Some(s)) => match axum::body::to_bytes(s, state.cfg.server.max_body_bytes).await {
            Ok(b) => b,
            Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        },
        (None, None) => Bytes::new(),
    };
    let path = path_and_query(&rest, &parts.uri);
    let fwd = forward_request_headers(&parts.headers);
    let label = model.map(|m| format!("{provider}/{m}"));
    state.subs.execute(SubProvider::OpencodeGo, parts.method, &path, fwd, bytes, principal.client_id(), label.as_deref()).await
}

#[cfg(test)]
mod tests;
