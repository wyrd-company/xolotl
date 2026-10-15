//! OAuth helpers shared by the Codex and Claude flows, plus the registry of
//! in-progress logins started from the CLI.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::SubProvider;

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn pkce() -> Pkce {
    let bytes: [u8; 96] = rand::rng().random();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill(&mut buf[..]);
    hex::encode(buf)
}

/// Decode a JWT payload without verifying it (we only read identity claims
/// from tokens we just received over TLS from the issuer).
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).or_else(|_| URL_SAFE.decode(payload)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Token endpoint response common to both providers.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<f64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub organization: Option<Value>,
}

impl TokenResponse {
    /// Absolute expiry; guards against a missing/zero/negative `expires_in`,
    /// which would otherwise cause a refresh loop.
    pub fn expires_at(&self, now: i64) -> i64 {
        let secs = self.expires_in.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(3600.0);
        now + secs as i64
    }
}

/// Error codes from token endpoints that mean the grant is dead and the
/// account needs to log in again (as opposed to transient failures).
pub fn is_terminal_grant_error(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 401 | 403) {
        return false;
    }
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str().map(str::to_string).or_else(|| e.get("code").and_then(Value::as_str).map(str::to_string)))
        })
        .unwrap_or_default();
    matches!(
        code.as_str(),
        "invalid_grant" | "refresh_token_invalidated" | "token_invalidated" | "refresh_token_reused" | "refresh_token_expired"
    ) || body.contains("invalid_grant")
}

/// A specific reuse rejection is evidence of a spent token, not proof of who spent it.
pub fn is_reused_grant_error(body: &str) -> bool {
    serde_json::from_str::<Value>(body).ok().is_some_and(|v| {
        let error = &v["error"];
        error.as_str().or_else(|| error.get("code").and_then(Value::as_str)) == Some("refresh_token_reused")
    })
}

/// Extract `(code, state)` from whatever the user pasted: a full callback URL,
/// `code#state`, or a bare code.
pub fn parse_pasted_code(input: &str) -> Option<(String, Option<String>)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input.contains("://")
        && let Ok(url) = url::Url::parse(input)
    {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        // Some flows put the state in the fragment.
        if state.is_none()
            && let Some(f) = url.fragment().filter(|f| !f.is_empty())
        {
            state = Some(f.to_string());
        }
        return code.map(|c| (c, state));
    }
    match input.split_once('#') {
        Some((c, s)) => Some((c.to_string(), Some(s.to_string()).filter(|s| !s.is_empty()))),
        None => Some((input.to_string(), None)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FlowStatus {
    Pending,
    Complete { account_id: String, email: Option<String> },
    Failed { error: String },
}

#[derive(Debug, Clone)]
pub enum FlowKind {
    /// Authorization-code + PKCE; the user pastes the code or callback URL.
    Pkce { verifier: String, state: String, redirect_uri: String },
    /// Codex device-code; the server polls in the background.
    Device,
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub provider: SubProvider,
    pub kind: FlowKind,
    pub status: FlowStatus,
    pub created: Instant,
}

/// Public description of a started flow returned to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStart {
    pub flow_id: String,
    pub provider: SubProvider,
    /// Where the user should go.
    pub url: String,
    /// Device flows: the code to enter at `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    /// PKCE flows need the code pasted back via `complete`.
    pub needs_code: bool,
    pub instructions: String,
}

const FLOW_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Default)]
pub struct Flows {
    inner: Mutex<HashMap<String, Flow>>,
}

impl Flows {
    pub fn insert(&self, provider: SubProvider, kind: FlowKind) -> String {
        let id = random_hex(12);
        let mut m = self.inner.lock();
        m.retain(|_, f| f.created.elapsed() < FLOW_TTL);
        m.insert(id.clone(), Flow { provider, kind, status: FlowStatus::Pending, created: Instant::now() });
        id
    }
    pub fn get(&self, id: &str) -> Option<Flow> {
        self.inner.lock().get(id).filter(|f| f.created.elapsed() < FLOW_TTL).cloned()
    }
    pub fn set_status(&self, id: &str, status: FlowStatus) {
        if let Some(f) = self.inner.lock().get_mut(id) {
            f.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_code_variants() {
        assert_eq!(parse_pasted_code("abc#xyz"), Some(("abc".into(), Some("xyz".into()))));
        assert_eq!(parse_pasted_code(" abc "), Some(("abc".into(), None)));
        assert_eq!(parse_pasted_code("http://localhost:1455/auth/callback?code=c1&state=s1"), Some(("c1".into(), Some("s1".into()))));
        assert_eq!(parse_pasted_code("http://localhost:1455/auth/callback?error=denied"), None);
        assert_eq!(parse_pasted_code(""), None);
    }

    #[test]
    fn jwt_decode() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"a@b.c"}"#);
        let token = format!("h.{payload}.s");
        assert_eq!(jwt_claims(&token).unwrap()["email"], "a@b.c");
        assert!(jwt_claims("garbage").is_none());
    }

    #[test]
    fn expiry_guard() {
        let t = |e: Option<f64>| TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            id_token: None,
            expires_in: e,
            scope: None,
            account: None,
            organization: None,
        };
        assert_eq!(t(None).expires_at(0), 3600);
        assert_eq!(t(Some(0.0)).expires_at(0), 3600);
        assert_eq!(t(Some(28800.0)).expires_at(10), 28810);
    }

    #[test]
    fn terminal_errors() {
        assert!(is_terminal_grant_error(400, r#"{"error":"invalid_grant"}"#));
        assert!(is_terminal_grant_error(401, r#"{"error":{"code":"refresh_token_reused"}}"#));
        assert!(!is_terminal_grant_error(500, r#"{"error":"invalid_grant"}"#));
        assert!(!is_terminal_grant_error(400, r#"{"error":"slow_down"}"#));
    }

    #[test]
    fn pkce_shape() {
        let p = pkce();
        assert_eq!(p.verifier.len(), 128);
        assert_eq!(p.challenge.len(), 43);
    }
}
