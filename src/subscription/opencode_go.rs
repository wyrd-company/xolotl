//! OpenCode Go subscription: API-key accounts with monthly dollar limits and
//! 5h / weekly windows. Endpoint format depends on the model; clients pick
//! the matching path (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`).

use axum::http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Credential, Failure};
use crate::config::Endpoints;
use crate::db::UsageWindow;
use crate::proxy::set_header;

/// Any path under the base: the unified endpoints and the `/opencode-go/...`
/// passthrough both land here.
pub fn upstream_url(ep: &Endpoints, path: &str) -> Option<String> {
    path.starts_with('/').then(|| format!("{}{path}", ep.opencode_go.trim_end_matches('/')))
}

pub fn prepare(headers: &mut HeaderMap, body: &mut Value, cred: &Credential, path: &str, session: &str) {
    let key = cred.api_key.as_deref().unwrap_or_default();
    set_header(headers, "authorization", &format!("Bearer {key}"));
    if path.starts_with("/v1/messages") {
        set_header(headers, "x-api-key", key);
    }
    set_header(headers, "x-opencode-session", session);
    let agent = concat!("xolotl/", env!("CARGO_PKG_VERSION"));
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .map(|v| format!("{agent} ({v})"))
        .unwrap_or_else(|| agent.to_string());
    set_header(headers, "user-agent", &ua);
    if let Some(obj) = body.as_object_mut()
        && let Some(Value::String(m)) = obj.get("model")
        && let Some(stripped) = m.strip_prefix("opencode-go/")
    {
        obj.insert("model".into(), Value::String(stripped.to_string()));
    }
}

pub fn session_key(headers: &HeaderMap, client_id: Option<&str>) -> String {
    ["x-opencode-session", "x-claude-code-session-id", "session_id"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).filter(|v| !v.trim().is_empty()).map(str::to_string))
        .unwrap_or_else(|| format!("xolotl-{}", hex::encode(Sha256::digest(client_id.unwrap_or("anonymous").as_bytes()))))
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
        // A 403 can reject the request without invalidating the API key.
        401 => Failure::Auth,
        // Bursts return short 429s; long Retry-After means a window is exhausted.
        429 => match retry_after {
            Some(s) if s > 300 => Failure::Exhausted { until: Some(crate::db::now() + s) },
            _ => Failure::Transient { retry_after },
        },
        500 | 502 | 503 | 504 => Failure::Transient { retry_after },
        _ => Failure::Fatal,
    }
}

/// Keep diagnostic fields, not arbitrary response data. Redact the credential
/// before truncation so a key crossing the truncation boundary cannot leak.
pub fn error_reason(status: u16, bytes: &[u8], cred: &Credential) -> String {
    let val: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let detail = if val.is_null() {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let error = val.get("error").unwrap_or(&val);
        if let Some(text) = error.as_str() {
            text.to_string()
        } else {
            ["type", "code", "name", "message"]
                .iter()
                .filter_map(|field| error.get(*field).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(": ")
        }
    };
    let mut detail = detail;
    for secret in [cred.api_key.as_deref(), Some(cred.access_token.as_str()), cred.refresh_token.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
    {
        detail = detail.replace(secret, "[redacted]");
    }
    let detail: String = detail.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut reason = format!("upstream status {status}");
    if !detail.trim().is_empty() {
        reason.push_str(": ");
        reason.extend(detail.trim().chars().take(512));
        if detail.trim().chars().count() > 512 {
            reason.push('…');
        }
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_fallback_is_stable_and_client_specific() {
        let mut h = HeaderMap::new();
        let fallback = session_key(&h, Some("client-a"));
        assert_eq!(fallback, session_key(&h, Some("client-a")));
        assert_ne!(fallback, session_key(&h, Some("client-b")));
        assert!(!fallback.contains("client-a"));
        set_header(&mut h, "x-opencode-session", " ");
        assert_eq!(fallback, session_key(&h, Some("client-a")));
        set_header(&mut h, "session_id", "native");
        set_header(&mut h, "x-claude-code-session-id", "claude");
        assert_eq!(session_key(&h, None), "claude");
        set_header(&mut h, "x-opencode-session", "explicit");
        assert_eq!(session_key(&h, None), "explicit");
    }

    #[test]
    fn diagnostics_are_redacted_bounded_and_single_line() {
        let cred = Credential { api_key: Some("test-secret".into()), ..Default::default() };
        for bytes in [
            json!({"error": {"message": "refused\n test-secret", "api_key": "other-secret"}}).to_string(),
            "refused\n test-secret".into(),
            json!({"message": format!("{}test-secret{}", "é".repeat(510), "é".repeat(100))}).to_string(),
        ] {
            let reason = error_reason(403, bytes.as_bytes(), &cred);
            assert!(!reason.contains("test-secret"));
            assert!(!reason.contains("other-secret"));
            assert!(!reason.contains('\n'));
            assert!(reason.chars().count() <= 534);
        }
        assert_eq!(error_reason(403, b"", &cred), "upstream status 403");
        let boundary = format!("{}test-secret", "é".repeat(510));
        assert!(error_reason(403, boundary.as_bytes(), &cred).ends_with("[r…"));
    }

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
        prepare(&mut h, &mut b, &c, "/v1/messages", "session");
        assert_eq!(h["x-api-key"], "k");
        assert_eq!(h["x-opencode-session"], "session");
        assert_eq!(h["user-agent"], concat!("xolotl/", env!("CARGO_PKG_VERSION")));
        assert_eq!(b["model"], "minimax-m3");
    }
}
