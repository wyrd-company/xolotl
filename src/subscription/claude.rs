//! Claude subscription (Pro/Max) accounts: OAuth (PKCE with manual code
//! paste), refresh, request preparation for Claude Code passthrough, usage
//! and 429 classification.

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use super::oauth::TokenResponse;
use super::{Credential, Failure, RefreshError, SubProvider};
use crate::config::Endpoints;
use crate::db::{UsageWindow, now};
use crate::proxy::set_header;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
const DEFAULT_UA: &str = "claude-cli/2.1.280 (external, cli)";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn upstream_url(ep: &Endpoints, path: &str) -> Option<String> {
    let base = ep.claude_api.trim_end_matches('/');
    match path {
        "/v1/messages" => Some(format!("{base}/v1/messages?beta=true")),
        "/v1/messages/count_tokens" => Some(format!("{base}/v1/messages/count_tokens?beta=true")),
        "/v1/models" => Some(format!("{base}/v1/models")),
        _ => None,
    }
}

/// True when the caller is the real Claude Code CLI; its requests pass
/// through with only credential-scoped changes.
pub fn is_claude_code(headers: &HeaderMap) -> bool {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    h("x-app") == "cli" && h("user-agent").starts_with("claude-cli/")
}

pub fn prepare(headers: &mut HeaderMap, body: &mut Value, cred: &Credential, path: &str) {
    let native = is_claude_code(headers);
    set_header(headers, "authorization", &format!("Bearer {}", cred.access_token));
    if !headers.contains_key("anthropic-version") {
        set_header(headers, "anthropic-version", ANTHROPIC_VERSION);
    }

    // Merge betas: client header + body `betas` (upstream rejects it in the body) + required.
    let mut betas: Vec<String> = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if let Some(Value::Array(extra)) = body.as_object_mut().and_then(|o| o.remove("betas")) {
        betas.extend(extra.into_iter().filter_map(|b| b.as_str().map(str::to_string)));
    }
    for required in [OAUTH_BETA, CLAUDE_CODE_BETA] {
        if !betas.iter().any(|b| b == required) {
            betas.insert(0, required.to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    betas.retain(|b| seen.insert(b.clone()));
    set_header(headers, "anthropic-beta", &betas.join(","));

    if !native {
        set_header(headers, "x-app", "cli");
        set_header(headers, "user-agent", DEFAULT_UA);
        set_header(headers, "anthropic-dangerous-direct-browser-access", "true");
    }

    let Some(obj) = body.as_object_mut() else { return };
    if let Some(Value::String(m)) = obj.get("model")
        && let Some(stripped) = m.strip_prefix("claude/")
    {
        obj.insert("model".into(), Value::String(stripped.to_string()));
    }
    if path.ends_with("/count_tokens") {
        for k in ["metadata", "context_management", "diagnostics"] {
            obj.remove(k);
        }
        return;
    }
    if !native {
        ensure_identity(obj);
    }
    rewrite_user_id(obj, cred, headers);
}

/// Subscription tokens only work with the Claude Code system identity.
fn ensure_identity(obj: &mut Map<String, Value>) {
    let identity = json!({"type": "text", "text": IDENTITY});
    match obj.remove("system") {
        None | Some(Value::Null) => {
            obj.insert("system".into(), json!([identity]));
        }
        Some(Value::String(s)) => {
            let mut blocks = vec![identity];
            if !s.is_empty() {
                blocks.push(json!({"type": "text", "text": s}));
            }
            obj.insert("system".into(), Value::Array(blocks));
        }
        Some(Value::Array(mut blocks)) => {
            let present = blocks.iter().any(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.starts_with("You are Claude Code") || t.contains("x-anthropic-billing-header"))
            });
            if !present {
                blocks.insert(0, identity);
            }
            obj.insert("system".into(), Value::Array(blocks));
        }
        Some(other) => {
            obj.insert("system".into(), other);
        }
    }
}

/// `metadata.user_id` is a JSON-encoded string carrying device and account
/// identity; point it at the account actually serving the request.
fn rewrite_user_id(obj: &mut Map<String, Value>, cred: &Credential, headers: &HeaderMap) {
    let metadata = obj.entry("metadata").or_insert_with(|| json!({}));
    let Some(meta) = metadata.as_object_mut() else { return };
    let mut uid: Map<String, Value> = match meta.get("user_id").and_then(Value::as_str) {
        Some(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => m,
            // Unknown legacy format: leave the caller's value alone.
            _ => return,
        },
        None => Map::new(),
    };
    let session = uid
        .get("session_id")
        .cloned()
        .or_else(|| headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).map(|s| json!(s)))
        .unwrap_or_else(|| json!(uuid::Uuid::new_v4().to_string()));
    // Rebuild preserving the canonical key order, keeping any extra keys.
    let mut out = Map::new();
    out.insert("device_id".into(), json!(cred.device_id.clone().unwrap_or_default()));
    out.insert("account_uuid".into(), json!(cred.account_id.clone().unwrap_or_default()));
    out.insert("session_id".into(), session);
    for k in ["device_id", "account_uuid", "session_id"] {
        uid.remove(k);
    }
    out.extend(uid);
    meta.insert("user_id".into(), Value::String(Value::Object(out).to_string()));
}

pub fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| {
        let uid = body.pointer("/metadata/user_id")?.as_str()?;
        serde_json::from_str::<Value>(uid).ok()?.get("session_id")?.as_str().map(str::to_string)
    })
}

pub fn authorize_url(ep: &Endpoints, challenge: &str, state: &str) -> String {
    let mut u = url::Url::parse(&ep.claude_authorize).expect("valid url");
    u.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &ep.claude_manual_redirect)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    u.to_string()
}

pub async fn exchange_code(
    http: &reqwest::Client,
    ep: &Endpoints,
    code: &str,
    verifier: &str,
    state: &str,
) -> anyhow::Result<TokenResponse> {
    let resp = http
        .post(&ep.claude_token)
        .header("accept", "application/json")
        .json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": ep.claude_manual_redirect,
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
            "state": state,
        }))
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "token exchange failed: {status}: {text}");
    Ok(serde_json::from_str(&text)?)
}

pub async fn refresh(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> Result<TokenResponse, RefreshError> {
    let rt = cred.refresh_token.as_deref().ok_or(RefreshError::Terminal("no refresh token".into()))?;
    let resp = http
        .post(&ep.claude_token)
        .header("accept", "application/json")
        .json(&json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": rt}))
        .send()
        .await
        .map_err(RefreshError::from_send_error)?;
    RefreshError::from_response(resp).await
}

pub fn apply_tokens(cred: &mut Credential, tr: TokenResponse) {
    let t = now();
    cred.provider = SubProvider::Claude;
    cred.expires_at = Some(tr.expires_at(t));
    cred.last_refresh = Some(t);
    cred.access_token = tr.access_token.clone();
    if let Some(rt) = tr.refresh_token.clone().filter(|r| !r.is_empty()) {
        cred.refresh_token = Some(rt);
    }
    if let Some(scope) = tr.scope.clone() {
        cred.scope = Some(scope);
    }
    apply_identity(cred, tr.account.as_ref(), tr.organization.as_ref());
    if cred.device_id.is_none() {
        cred.device_id = Some(super::oauth::random_hex(32));
    }
}

fn apply_identity(cred: &mut Credential, account: Option<&Value>, org: Option<&Value>) {
    let s = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_str).map(str::to_string);
    if let Some(id) = s(account, "uuid") {
        cred.account_id = Some(id);
    }
    if let Some(email) = s(account, "email_address").or_else(|| s(account, "email")) {
        cred.email = Some(email.to_lowercase());
    }
    if let Some(org_id) = s(org, "uuid") {
        cred.organization_id = Some(org_id);
    }
    if let Some(plan) = s(org, "organization_type") {
        cred.plan = Some(plan);
    }
}

/// Fill identity from `/api/oauth/profile` (used for imported tokens).
pub async fn fetch_profile(http: &reqwest::Client, ep: &Endpoints, cred: &mut Credential) -> anyhow::Result<()> {
    let resp = http
        .get(format!("{}/api/oauth/profile", ep.claude_api.trim_end_matches('/')))
        .bearer_auth(&cred.access_token)
        .header("anthropic-beta", OAUTH_BETA)
        .header("user-agent", DEFAULT_UA)
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(status.is_success(), "profile request failed: {status}: {body}");
    apply_identity(cred, body.get("account"), body.get("organization"));
    Ok(())
}

/// Import Claude Code's `~/.claude/.credentials.json`.
pub fn import(data: &Value) -> anyhow::Result<Credential> {
    let o = data.get("claudeAiOauth").unwrap_or(data);
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    let access = s("accessToken").or_else(|| s("access_token")).ok_or_else(|| anyhow::anyhow!("no claudeAiOauth.accessToken"))?;
    let expires_ms = o.get("expiresAt").and_then(Value::as_i64);
    Ok(Credential {
        provider: SubProvider::Claude,
        access_token: access,
        refresh_token: s("refreshToken").or_else(|| s("refresh_token")),
        expires_at: expires_ms.map(|ms| ms / 1000),
        plan: s("subscriptionType"),
        device_id: Some(super::oauth::random_hex(32)),
        last_refresh: Some(now()),
        ..Default::default()
    })
}

pub fn models_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> reqwest::RequestBuilder {
    http.get(format!("{}/v1/models?limit=1000", ep.claude_api.trim_end_matches('/')))
        .bearer_auth(&cred.access_token)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", format!("{OAUTH_BETA},{CLAUDE_CODE_BETA}"))
        .header("user-agent", DEFAULT_UA)
}

pub fn usage_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> reqwest::RequestBuilder {
    http.get(format!("{}/api/oauth/usage", ep.claude_api.trim_end_matches('/')))
        .bearer_auth(&cred.access_token)
        .header("anthropic-beta", OAUTH_BETA)
        .header("user-agent", DEFAULT_UA)
        .header("accept", "application/json")
}

fn window_label(key: &str) -> String {
    match key {
        "five_hour" => "5h".into(),
        "seven_day" => "7d".into(),
        other => other.strip_prefix("seven_day_").map(|m| format!("7d-{m}")).unwrap_or_else(|| other.to_string()),
    }
}

/// Main (account-wide) windows; model-scoped windows don't bench the account.
pub fn is_account_window(name: &str) -> bool {
    matches!(name, "5h" | "7d")
}

fn parse_time(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp()),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// Parse `/api/oauth/usage` (utilization is a percent 0-100).
pub fn parse_usage(body: &Value) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    let Some(obj) = body.as_object() else { return out };
    for (key, w) in obj {
        let Some(used) = w.get("utilization").and_then(Value::as_f64).filter(|f| f.is_finite()) else { continue };
        let window_secs = match key.as_str() {
            "five_hour" => Some(5 * 3600),
            k if k.starts_with("seven_day") => Some(7 * 86_400),
            _ => None,
        };
        out.push(UsageWindow {
            name: window_label(key),
            used_percent: used.clamp(0.0, 100.0),
            resets_at: parse_time(w.get("resets_at")),
            window_secs,
        });
    }
    out.sort_by(|a, b| a.window_secs.cmp(&b.window_secs).then(a.name.cmp(&b.name)));
    out
}

fn header<'a>(h: &'a reqwest::header::HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok())
}

/// `anthropic-ratelimit-unified-*` response headers (utilization is a fraction 0-1).
pub fn windows_from_headers(h: &reqwest::header::HeaderMap) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    for (key, name, secs) in [("5h", "5h", 5 * 3600), ("7d", "7d", 7 * 86_400)] {
        let Some(util) = header(h, &format!("anthropic-ratelimit-unified-{key}-utilization")).and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        out.push(UsageWindow {
            name: name.into(),
            used_percent: (util * 100.0).clamp(0.0, 100.0),
            resets_at: header(h, &format!("anthropic-ratelimit-unified-{key}-reset")).and_then(|v| v.parse().ok()),
            window_secs: Some(secs),
        });
    }
    out
}

pub fn classify(status: u16, h: &reqwest::header::HeaderMap, body: &Value, retry_after: Option<i64>) -> Failure {
    let message = body.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let kind = body.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
    if status == 401 || (status == 403 && (message.contains("oauth") || message.contains("revoked"))) {
        return Failure::Auth;
    }
    if status == 429 {
        // Fast-mode entitlement errors are the caller's problem, not the account's.
        if message.contains("fast") && (message.contains("rejected") || message.contains("credit")) {
            return Failure::Fatal;
        }
        let st = |k: &str| header(h, &format!("anthropic-ratelimit-unified-{k}"));
        let reset = |k: &str| st(&format!("{k}-reset")).and_then(|v| v.parse::<i64>().ok());
        let rejected = |k: &str| st(&format!("{k}-status")) == Some("rejected");
        let t = now();
        let horizon = t + 7 * 86_400 + 3600;
        let mut deadlines = Vec::new();
        if rejected("5h") {
            deadlines.extend(reset("5h"));
        }
        if rejected("7d") {
            deadlines.extend(reset("7d"));
        }
        let account_wide = rejected("5h") || rejected("7d");
        let overage_only = !account_wide && (rejected("7d_oi") || st("overage-status") == Some("rejected"));
        if overage_only {
            // Model-family / overage limit: another account may not help and the
            // reset can be a billing boundary. Return it to the caller.
            return Failure::Fatal;
        }
        if account_wide || st("status") == Some("rejected") {
            if !account_wide {
                deadlines.extend(st("reset").and_then(|v| v.parse::<i64>().ok()));
            }
            deadlines.extend(retry_after.map(|s| t + s));
            let until = deadlines.into_iter().filter(|d| *d > t && *d < horizon).max();
            return Failure::Exhausted { until };
        }
        return Failure::Transient { retry_after };
    }
    if matches!(status, 500 | 502 | 503 | 504 | 529) || kind == "overloaded_error" {
        return Failure::Transient { retry_after };
    }
    Failure::Fatal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-app", "cli".parse().unwrap());
        h.insert("user-agent", "claude-cli/2.3.0 (external, cli)".parse().unwrap());
        h.insert("anthropic-beta", "claude-code-20250219,interleaved-thinking-2025-05-14".parse().unwrap());
        h
    }

    fn cred() -> Credential {
        Credential {
            provider: SubProvider::Claude,
            access_token: "sk-ant-oat".into(),
            account_id: Some("acct-uuid".into()),
            device_id: Some("dev".into()),
            ..Default::default()
        }
    }

    #[test]
    fn claude_code_passthrough_adds_oauth_beta_and_rewrites_user_id() {
        let mut h = cc_headers();
        let uid = json!({"device_id": "orig", "account_uuid": "other", "session_id": "sess", "extra": 1}).to_string();
        let mut b = json!({"model": "claude-opus-5", "system": [{"type": "text", "text": "custom"}], "metadata": {"user_id": uid}});
        prepare(&mut h, &mut b, &cred(), "/v1/messages");
        let beta = h["anthropic-beta"].to_str().unwrap();
        assert!(beta.contains("oauth-2025-04-20") && beta.contains("interleaved-thinking"));
        assert_eq!(h["user-agent"], "claude-cli/2.3.0 (external, cli)", "native UA untouched");
        assert_eq!(b["system"][0]["text"], "custom", "native system untouched");
        let uid: Value = serde_json::from_str(b["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(uid["account_uuid"], "acct-uuid");
        assert_eq!(uid["device_id"], "dev");
        assert_eq!(uid["session_id"], "sess");
        assert_eq!(uid["extra"], 1);
        assert_eq!(session_key(&h, &b).as_deref(), Some("sess"));
    }

    #[test]
    fn other_clients_get_identity() {
        let mut h = HeaderMap::new();
        let mut b = json!({"model": "claude/claude-sonnet-5", "system": "be brief", "betas": ["x-beta"]});
        prepare(&mut h, &mut b, &cred(), "/v1/messages");
        assert_eq!(b["model"], "claude-sonnet-5");
        assert_eq!(b["system"][0]["text"], IDENTITY);
        assert_eq!(b["system"][1]["text"], "be brief");
        assert!(b.get("betas").is_none());
        assert!(h["anthropic-beta"].to_str().unwrap().contains("x-beta"));
        assert_eq!(h["x-app"], "cli");
    }

    #[test]
    fn count_tokens_drops_metadata() {
        let mut h = cc_headers();
        let mut b = json!({"model": "m", "metadata": {"user_id": "x"}});
        prepare(&mut h, &mut b, &cred(), "/v1/messages/count_tokens");
        assert!(b.get("metadata").is_none());
    }

    #[test]
    fn usage_parsing() {
        let w = parse_usage(&json!({
            "five_hour": {"utilization": 54.0, "resets_at": "2026-07-18T14:39:00Z"},
            "seven_day": {"utilization": 18.4, "resets_at": null},
            "seven_day_opus": null,
            "extra_usage": {"is_enabled": false, "utilization": null}
        }));
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].name, "5h");
        assert!(w[0].resets_at.is_some());
        assert_eq!(w[1].name, "7d");
    }

    #[test]
    fn classify_429s() {
        let t = now();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-5h-status", "rejected".parse().unwrap());
        h.insert("anthropic-ratelimit-unified-5h-reset", (t + 1000).to_string().parse().unwrap());
        assert!(matches!(classify(429, &h, &json!({}), None), Failure::Exhausted { until: Some(u) } if u == t + 1000));

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-7d_oi-status", "rejected".parse().unwrap());
        assert!(matches!(classify(429, &h, &json!({}), None), Failure::Fatal));

        let h = reqwest::header::HeaderMap::new();
        assert!(matches!(classify(429, &h, &json!({}), Some(3)), Failure::Transient { retry_after: Some(3) }));
        assert!(matches!(classify(529, &h, &json!({}), None), Failure::Transient { .. }));
        assert!(matches!(classify(401, &h, &json!({}), None), Failure::Auth));
        assert!(matches!(classify(400, &h, &json!({}), None), Failure::Fatal));
    }
}
