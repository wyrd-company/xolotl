//! ChatGPT subscription (Codex) accounts: OAuth (device code and PKCE),
//! refresh, request preparation, usage and error classification.

use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::oauth::{TokenResponse, jwt_claims};
use super::{Credential, Failure, RefreshError, SubProvider};
use crate::config::Endpoints;
use crate::db::{UsageWindow, now};
use crate::proxy::set_header;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const BROWSER_REDIRECT: &str = "http://localhost:1455/auth/callback";
const SCOPES: &str = "openid profile email offline_access";
const ORIGINATOR: &str = "codex_cli_rs";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

pub fn upstream_url(ep: &Endpoints, path: &str) -> Option<String> {
    let base = ep.codex_backend.trim_end_matches('/');
    match path {
        "/v1/responses" => Some(format!("{base}/codex/responses")),
        "/v1/responses/compact" => Some(format!("{base}/codex/responses/compact")),
        _ => None,
    }
}

/// Prepare a Responses request for the ChatGPT backend. Codex CLI requests
/// already have the right shape; we only fix what the backend rejects.
pub fn prepare(headers: &mut HeaderMap, body: &mut Value, cred: &Credential, path: &str) {
    set_header(headers, "authorization", &format!("Bearer {}", cred.access_token));
    if let Some(acct) = &cred.account_id {
        set_header(headers, "chatgpt-account-id", acct);
    }
    if !headers.contains_key("originator") {
        set_header(headers, "originator", ORIGINATOR);
    }
    let compact = path.ends_with("/compact");
    set_header(headers, "accept", if compact { "application/json" } else { "text/event-stream" });
    set_header(headers, "content-type", "application/json");

    if let Some(obj) = body.as_object_mut() {
        if let Some(Value::String(m)) = obj.get("model")
            && let Some(stripped) = m.strip_prefix("codex/")
        {
            obj.insert("model".into(), Value::String(stripped.to_string()));
        }
        // The backend is stateless (store=false): server-side references fail.
        obj.remove("previous_response_id");
        obj.insert("store".into(), Value::Bool(false));
        if compact {
            obj.remove("stream");
        } else {
            obj.insert("stream".into(), Value::Bool(true));
        }
        if !obj.contains_key("instructions") {
            obj.insert("instructions".into(), Value::String(String::new()));
        }
        // A plain-string input is accepted by the public API but not the backend.
        if let Some(Value::String(text)) = obj.get("input").cloned() {
            obj.insert("input".into(), json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]));
        }
        for k in ["max_output_tokens", "temperature", "top_p", "truncation", "user", "safety_identifier"] {
            obj.remove(k);
        }
    }
}

pub fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    ["session_id", "session-id", "thread-id"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).map(str::to_string))
        .or_else(|| body.get("prompt_cache_key").and_then(Value::as_str).map(str::to_string))
}

pub fn authorize_url(ep: &Endpoints, challenge: &str, state: &str) -> String {
    let mut u = url::Url::parse(&format!("{}/oauth/authorize", ep.codex_auth.trim_end_matches('/'))).expect("valid url");
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", BROWSER_REDIRECT)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    u.to_string()
}

pub fn device_verification_url(ep: &Endpoints) -> String {
    format!("{}/codex/device", ep.codex_auth.trim_end_matches('/'))
}

pub fn device_redirect(ep: &Endpoints) -> String {
    format!("{}/deviceauth/callback", ep.codex_auth.trim_end_matches('/'))
}

pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

pub async fn start_device(http: &reqwest::Client, ep: &Endpoints) -> anyhow::Result<DeviceCode> {
    let url = format!("{}/api/accounts/deviceauth/usercode", ep.codex_auth.trim_end_matches('/'));
    let resp = http.post(&url).json(&json!({"client_id": CLIENT_ID})).send().await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(status.is_success(), "device code request failed: {status}: {body}");
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let interval = match body.get("interval") {
        Some(Value::String(s)) => s.parse().unwrap_or(5),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(5),
        _ => 5,
    };
    Ok(DeviceCode {
        device_auth_id: s("device_auth_id").ok_or_else(|| anyhow::anyhow!("missing device_auth_id"))?,
        user_code: s("user_code").or_else(|| s("usercode")).ok_or_else(|| anyhow::anyhow!("missing user_code"))?,
        interval: interval.clamp(1, 60),
    })
}

/// One device-code poll: `Ok(None)` while the user has not finished.
pub async fn poll_device(http: &reqwest::Client, ep: &Endpoints, dc: &DeviceCode) -> anyhow::Result<Option<(String, String)>> {
    let url = format!("{}/api/accounts/deviceauth/token", ep.codex_auth.trim_end_matches('/'));
    let resp = http.post(&url).json(&json!({"device_auth_id": dc.device_auth_id, "user_code": dc.user_code})).send().await?;
    let status = resp.status().as_u16();
    if status == 403 || status == 404 {
        return Ok(None);
    }
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    anyhow::ensure!((200..300).contains(&status), "device token poll failed: {status}: {body}");
    let code = body.get("authorization_code").and_then(Value::as_str);
    let verifier = body.get("code_verifier").and_then(Value::as_str);
    match (code, verifier) {
        (Some(c), Some(v)) => Ok(Some((c.to_string(), v.to_string()))),
        _ => anyhow::bail!("device token response missing authorization_code/code_verifier"),
    }
}

pub async fn exchange_code(
    http: &reqwest::Client,
    ep: &Endpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<TokenResponse> {
    let url = format!("{}/oauth/token", ep.codex_auth.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "token exchange failed: {status}: {text}");
    Ok(serde_json::from_str(&text)?)
}

pub async fn refresh(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> Result<TokenResponse, RefreshError> {
    let rt = cred.refresh_token.as_deref().ok_or(RefreshError::Terminal("no refresh token".into()))?;
    let url = format!("{}/oauth/token", ep.codex_auth.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", rt), ("scope", "openid profile email")])
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    RefreshError::from_response(resp).await
}

/// Build/update a credential from a token response. Identity claims missing
/// from a refreshed id_token keep their previous values.
pub fn apply_tokens(cred: &mut Credential, tr: TokenResponse) {
    let t = now();
    cred.provider = SubProvider::Codex;
    cred.expires_at = Some(tr.expires_at(t));
    cred.last_refresh = Some(t);
    if let Some(rt) = tr.refresh_token.filter(|r| !r.is_empty()) {
        cred.refresh_token = Some(rt);
    }
    for token in [tr.id_token.as_deref(), Some(tr.access_token.as_str())].into_iter().flatten() {
        if let Some(claims) = jwt_claims(token) {
            let auth = claims.get(AUTH_CLAIM);
            let get = |k: &str| auth.and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
            if cred.account_id.is_none() || get("chatgpt_account_id").is_some() {
                cred.account_id = get("chatgpt_account_id").or(cred.account_id.take());
            }
            if let Some(plan) = get("chatgpt_plan_type") {
                cred.plan = Some(plan);
            }
            if let Some(email) = claims.get("email").and_then(Value::as_str) {
                cred.email = Some(email.to_lowercase());
            }
        }
    }
    if let Some(id) = tr.id_token.filter(|t| !t.is_empty()) {
        cred.id_token = Some(id);
    }
    cred.access_token = tr.access_token;
}

/// Import Codex CLI's `~/.codex/auth.json`.
pub fn import(data: &Value) -> anyhow::Result<Credential> {
    let tokens = data.get("tokens").unwrap_or(data);
    let s = |k: &str| tokens.get(k).and_then(Value::as_str).map(str::to_string);
    let access = s("access_token").ok_or_else(|| anyhow::anyhow!("auth.json has no tokens.access_token"))?;
    let exp = jwt_claims(&access).and_then(|c| c.get("exp").and_then(Value::as_i64));
    let mut cred = Credential { provider: SubProvider::Codex, account_id: s("account_id"), ..Default::default() };
    apply_tokens(
        &mut cred,
        TokenResponse {
            access_token: access,
            refresh_token: s("refresh_token"),
            id_token: s("id_token"),
            expires_in: exp.map(|e| (e - now()) as f64),
            scope: None,
            account: None,
            organization: None,
        },
    );
    if exp.is_some_and(|e| e <= now()) {
        cred.expires_at = Some(0); // force refresh on first use
    }
    Ok(cred)
}

fn usage_headers(cred: &Credential) -> Vec<(&'static str, String)> {
    let mut h = vec![("authorization", format!("Bearer {}", cred.access_token)), ("accept", "application/json".into())];
    if let Some(a) = &cred.account_id {
        h.push(("chatgpt-account-id", a.clone()));
    }
    h
}

pub fn usage_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> reqwest::RequestBuilder {
    let mut req = http.get(format!("{}/wham/usage", ep.codex_backend.trim_end_matches('/')));
    for (k, v) in usage_headers(cred) {
        req = req.header(k, v);
    }
    req
}

/// The model roster; upstream filters it by the client version it is told.
pub fn models_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential, client_version: &str) -> reqwest::RequestBuilder {
    let mut url = url::Url::parse(&format!("{}/codex/models", ep.codex_backend.trim_end_matches('/'))).expect("valid url");
    url.query_pairs_mut().append_pair("client_version", client_version);
    let mut req = http.get(url);
    for (k, v) in usage_headers(cred) {
        req = req.header(k, v);
    }
    req
}

/// `{"models":[{"slug","supported_in_api","visibility"}]}` -> usable slugs.
pub fn parse_models(body: &Value) -> Vec<String> {
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("supported_in_api").and_then(Value::as_bool).unwrap_or(true))
        .filter(|m| m.get("visibility").and_then(Value::as_str) != Some("hide"))
        .filter_map(|m| m.get("slug").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub fn window_name(secs: Option<i64>, fallback: &str) -> String {
    match secs {
        Some(s) if s >= 28 * 86_400 => "monthly".into(),
        Some(s) if s >= 86_400 => format!("{}d", s / 86_400),
        Some(s) if s >= 3600 => format!("{}h", s / 3600),
        Some(s) if s > 0 => format!("{}m", s / 60),
        _ => fallback.into(),
    }
}

/// Parse `/wham/usage`. Returns the windows and the plan type.
pub fn parse_usage(body: &Value) -> (Vec<UsageWindow>, Option<String>) {
    let mut out = Vec::new();
    for (key, fallback) in [("primary_window", "primary"), ("secondary_window", "secondary"), ("tertiary_window", "tertiary")] {
        let Some(w) = body.pointer(&format!("/rate_limit/{key}")).filter(|w| w.is_object()) else { continue };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64).filter(|f| f.is_finite()) else { continue };
        let window_secs = w.get("limit_window_seconds").and_then(Value::as_i64);
        let resets_at = w
            .get("reset_at")
            .and_then(Value::as_i64)
            .map(|r| if r > 10_000_000_000 { r / 1000 } else { r })
            .or_else(|| w.get("reset_after_seconds").and_then(Value::as_i64).map(|s| now() + s));
        out.push(UsageWindow { name: window_name(window_secs, fallback), used_percent: used.clamp(0.0, 100.0), resets_at, window_secs });
    }
    (out, body.get("plan_type").and_then(Value::as_str).map(str::to_string))
}

/// Usage carried on every response (`x-codex-primary-used-percent` etc.).
/// Note the window here is in minutes, unlike `/wham/usage`.
pub fn windows_from_headers(h: &reqwest::header::HeaderMap) -> Vec<UsageWindow> {
    let get = |k: String| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut out = Vec::new();
    for which in ["primary", "secondary"] {
        let Some(used) = get(format!("x-codex-{which}-used-percent")).and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        let window_secs = get(format!("x-codex-{which}-window-minutes")).and_then(|v| v.parse::<i64>().ok()).map(|m| m * 60);
        let resets_at = get(format!("x-codex-{which}-reset-at"))
            .and_then(|v| v.parse::<i64>().ok())
            .or_else(|| get(format!("x-codex-{which}-reset-after-seconds")).and_then(|v| v.parse::<i64>().ok()).map(|s| now() + s));
        out.push(UsageWindow { name: window_name(window_secs, which), used_percent: used.clamp(0.0, 100.0), resets_at, window_secs });
    }
    out
}

/// Classify an error body (HTTP error or an `error` / `response.failed`
/// event at the start of a 200 stream).
pub fn classify(status: u16, body: &Value, retry_after: Option<i64>) -> Failure {
    let err = body.get("error").filter(|e| e.is_object()).or_else(|| body.pointer("/response/error")).unwrap_or(body);
    let kind = [err.get("type"), err.get("code"), body.get("type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let has = |s: &str| kind.iter().any(|k| k == s);
    let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    if has("usage_limit_reached") {
        let t = now();
        let until = err
            .get("resets_at")
            .and_then(Value::as_i64)
            .filter(|r| *r > t)
            .or_else(|| err.get("resets_in_seconds").and_then(Value::as_i64).filter(|s| *s > 0).map(|s| t + s))
            .map(|u| u.max(t + 10));
        return Failure::Exhausted { until };
    }
    // Request-shaped failures would fail on every account: return them.
    if has("context_length_exceeded") || has("invalid_request_error") || message.contains("context window") {
        return Failure::Fatal;
    }
    if status == 401 || has("token_expired") || has("invalid_api_key") {
        return Failure::Auth;
    }
    if has("rate_limit_exceeded") || has("rate_limit_error") || has("server_is_overloaded") || message.contains("at capacity") {
        return Failure::Transient { retry_after };
    }
    match status {
        429 => Failure::Transient { retry_after },
        500 | 502 | 503 | 504 | 520..=527 => Failure::Transient { retry_after },
        200..=299 if has("error") || has("response.failed") => Failure::Transient { retry_after },
        _ => Failure::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn jwt(claims: Value) -> String {
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn apply_tokens_extracts_identity_and_keeps_old_on_missing() {
        let mut c = Credential::default();
        apply_tokens(
            &mut c,
            TokenResponse {
                access_token: "at".into(),
                refresh_token: Some("rt".into()),
                id_token: Some(jwt(json!({"email": "A@B.c", AUTH_CLAIM: {"chatgpt_account_id": "acct", "chatgpt_plan_type": "pro"}}))),
                expires_in: Some(100.0),
                scope: None,
                account: None,
                organization: None,
            },
        );
        assert_eq!(c.account_id.as_deref(), Some("acct"));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert_eq!(c.email.as_deref(), Some("a@b.c"));
        // Refresh without id_token or rotated refresh token keeps identity.
        apply_tokens(
            &mut c,
            TokenResponse {
                access_token: "at2".into(),
                refresh_token: None,
                id_token: None,
                expires_in: None,
                scope: None,
                account: None,
                organization: None,
            },
        );
        assert_eq!(c.access_token, "at2");
        assert_eq!(c.refresh_token.as_deref(), Some("rt"));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert_eq!(c.email.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn usage_parsing() {
        let body = json!({"plan_type": "plus", "rate_limit": {
            "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_at": 1790000000},
            "secondary_window": {"used_percent": 140, "limit_window_seconds": 604800, "reset_at": 1790500000},
            "tertiary_window": null}});
        let (w, plan) = parse_usage(&body);
        assert_eq!(plan.as_deref(), Some("plus"));
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].name, "5h");
        assert_eq!(w[1].name, "7d");
        assert_eq!(w[1].used_percent, 100.0);
    }

    #[test]
    fn model_roster() {
        let body = json!({"models": [
            {"slug": "gpt-6-codex", "supported_in_api": true},
            {"slug": "hidden", "supported_in_api": true, "visibility": "hide"},
            {"slug": "internal", "supported_in_api": false}]});
        assert_eq!(parse_models(&body), vec!["gpt-6-codex"]);
    }

    #[test]
    fn header_windows_use_minutes() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-codex-primary-used-percent", "42.5".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert("x-codex-primary-reset-at", "1790000000".parse().unwrap());
        let w = windows_from_headers(&h);
        assert_eq!(w[0].name, "5h");
        assert_eq!(w[0].window_secs, Some(18000));
    }

    #[test]
    fn classify_usage_limit() {
        let t = now();
        let f = classify(429, &json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}}), None);
        assert!(matches!(f, Failure::Exhausted { until: Some(u) } if u >= t + 3599));
        let f = classify(200, &json!({"type": "error", "error": {"type": "usage_limit_reached"}}), None);
        assert!(matches!(f, Failure::Exhausted { until: None }));
        assert!(matches!(classify(429, &json!({"error": {"code": "rate_limit_exceeded"}}), Some(5)), Failure::Transient { .. }));
        assert!(matches!(classify(401, &json!({}), None), Failure::Auth));
        assert!(matches!(classify(400, &json!({"error": {"message": "bad"}}), None), Failure::Fatal));
        let failed = json!({"type": "response.failed", "response": {"error": {"code": "context_length_exceeded"}}});
        assert!(matches!(classify(200, &failed, None), Failure::Fatal));
    }

    #[test]
    fn prepare_rewrites_body() {
        let mut h = HeaderMap::new();
        let mut b = json!({"model": "codex/gpt-5-codex", "input": "hi", "previous_response_id": "x", "temperature": 1});
        let c = Credential { access_token: "tok".into(), account_id: Some("acct".into()), ..Default::default() };
        prepare(&mut h, &mut b, &c, "/v1/responses");
        assert_eq!(b["model"], "gpt-5-codex");
        assert_eq!(b["stream"], true);
        assert_eq!(b["store"], false);
        assert!(b.get("previous_response_id").is_none());
        assert!(b.get("temperature").is_none());
        assert_eq!(b["input"][0]["content"][0]["text"], "hi");
        assert_eq!(h["authorization"], "Bearer tok");
        assert_eq!(h["chatgpt-account-id"], "acct");
    }
}
