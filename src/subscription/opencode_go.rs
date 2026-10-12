//! OpenCode Go subscription: API-key accounts with monthly dollar limits and
//! 5h / weekly windows. Endpoint format depends on the model; clients pick
//! the matching path (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`).

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Credential, Failure};
use crate::config::Endpoints;
use crate::db::UsageWindow;
use crate::proxy::set_header;

pub fn upstream_url(ep: &Endpoints, path: &str) -> Option<String> {
    let base = ep.opencode_go.trim_end_matches('/');
    match path {
        "/v1/chat/completions" | "/v1/messages" | "/v1/messages/count_tokens" | "/v1/responses" | "/v1/models" => {
            Some(format!("{base}{path}"))
        }
        _ => None,
    }
}

pub fn prepare(headers: &mut HeaderMap, body: &mut Value, cred: &Credential, path: &str) {
    let key = cred.api_key.as_deref().unwrap_or_default();
    set_header(headers, "authorization", &format!("Bearer {key}"));
    if path.starts_with("/v1/messages") {
        set_header(headers, "x-api-key", key);
    }
    if !headers.contains_key("user-agent") {
        set_header(headers, "user-agent", concat!("xolotl/", env!("CARGO_PKG_VERSION")));
    }
    if let Some(obj) = body.as_object_mut()
        && let Some(Value::String(m)) = obj.get("model")
        && let Some(stripped) = m.strip_prefix("opencode-go/")
    {
        obj.insert("model".into(), Value::String(stripped.to_string()));
    }
}

pub fn session_key(headers: &HeaderMap) -> Option<String> {
    ["x-opencode-session", "x-claude-code-session-id", "session_id"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).map(str::to_string))
}

pub fn usage_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> reqwest::RequestBuilder {
    http.get(format!("{}/v1/usage", ep.opencode_go.trim_end_matches('/')))
        .bearer_auth(cred.api_key.as_deref().unwrap_or_default())
        .header("accept", "application/json")
}

/// `{"usage":{"rolling":{percent,resetsAt},"weekly":{..},"monthly":{..}}}`.
pub fn parse_usage(body: &Value) -> Vec<UsageWindow> {
    let mut out = Vec::new();
    for (key, name, secs) in [("rolling", "5h", 5 * 3600), ("weekly", "7d", 7 * 86_400), ("monthly", "monthly", 30 * 86_400)] {
        let Some(w) = body.pointer(&format!("/usage/{key}")) else { continue };
        let Some(pct) = w.get("percent").and_then(Value::as_f64).filter(|f| f.is_finite()) else { continue };
        let resets_at =
            w.get("resetsAt").and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp());
        out.push(UsageWindow { name: name.into(), used_percent: pct.clamp(0.0, 100.0), resets_at, window_secs: Some(secs) });
    }
    out
}

pub fn classify(status: u16, retry_after: Option<i64>) -> Failure {
    match status {
        401 | 403 => Failure::Auth,
        // Bursts return short 429s; long Retry-After means a window is exhausted.
        429 => match retry_after {
            Some(s) if s > 300 => Failure::Exhausted { until: Some(crate::db::now() + s) },
            _ => Failure::Transient { retry_after },
        },
        500 | 502 | 503 | 504 => Failure::Transient { retry_after },
        _ => Failure::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_parsing() {
        let w = parse_usage(&json!({"usage": {
            "rolling": {"status": "ok", "percent": 12, "resetsAt": "2026-08-12T20:00:00.000Z"},
            "weekly": {"status": "ok", "percent": 8, "resetsAt": "2026-08-15T20:00:00.000Z"},
            "monthly": {"status": "ok", "percent": 35}}}));
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].name, "5h");
        assert!(w[0].resets_at.is_some());
        assert_eq!(w[2].used_percent, 35.0);
    }

    #[test]
    fn messages_get_x_api_key() {
        let mut h = HeaderMap::new();
        let mut b = json!({"model": "opencode-go/minimax-m3"});
        let c = Credential { api_key: Some("k".into()), ..Default::default() };
        prepare(&mut h, &mut b, &c, "/v1/messages");
        assert_eq!(h["x-api-key"], "k");
        assert_eq!(b["model"], "minimax-m3");
    }
}
