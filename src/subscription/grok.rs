//! Grok Build (xAI) subscription accounts: OIDC device-code login, refresh,
//! request preparation for xAI's CLI chat proxy, keepalive rewriting and
//! error classification. The proxy speaks Responses and streams SSE.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::HeaderMap;
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;

use super::oauth::{TokenResponse, jwt_claims};
use super::{Credential, Failure, RefreshError, SubProvider};
use crate::config::Endpoints;
use crate::db::now;
use crate::proxy::set_header;

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const CLIENT_IDENTIFIER: &str = "grok-shell";
/// The proxy's free tier resets over a rolling 24 hours.
const FREE_USAGE_COOLDOWN_SECS: i64 = 24 * 3600;
const KEEPALIVE_COMMENT: &[u8] = b": keepalive\n\n";

/// Identity headers xolotl sets itself; a client's copies are dropped.
const IDENTITY_HEADERS: &[&str] =
    &["x-xai-token-auth", "x-grok-client-version", "x-grok-client-identifier", "x-authenticateresponse", "user-agent"];

pub fn upstream_url(ep: &Endpoints, path: &str) -> Option<String> {
    let base = ep.grok_api.trim_end_matches('/');
    match path {
        "/v1/responses" => Some(format!("{base}/responses")),
        _ => None,
    }
}

/// True when the caller is the Grok CLI itself.
pub fn is_grok_shell(headers: &HeaderMap) -> bool {
    headers.get("user-agent").and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.to_ascii_lowercase().contains("grok-shell"))
}

/// The client version sent upstream: a Grok CLI's own, otherwise the configured one.
pub fn client_version<'a>(headers: &'a HeaderMap, configured: &'a str) -> &'a str {
    if is_grok_shell(headers)
        && let Some(v) = headers.get("x-grok-client-version").and_then(|v| v.to_str().ok()).filter(|v| !v.trim().is_empty())
    {
        return v;
    }
    configured
}

/// Prepare a Responses request for the CLI chat proxy.
pub fn prepare(headers: &mut HeaderMap, body: &mut Value, cred: &Credential, configured_version: &str) {
    let version = client_version(headers, configured_version).to_string();
    let user_agent =
        if is_grok_shell(headers) { headers.get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string) } else { None };
    for h in IDENTITY_HEADERS {
        headers.remove(*h);
    }
    set_header(headers, "authorization", &format!("Bearer {}", cred.access_token));
    set_header(headers, "content-type", "application/json");
    set_header(headers, "accept", "text/event-stream");
    set_header(headers, "x-xai-token-auth", "xai-grok-cli");
    set_header(headers, "x-grok-client-identifier", CLIENT_IDENTIFIER);
    set_header(headers, "x-authenticateresponse", "authenticate-response");
    set_header(headers, "x-grok-client-version", &version);
    set_header(headers, "user-agent", &user_agent.unwrap_or_else(|| format!("xai-grok-workspace/{version}")));

    let Some(obj) = body.as_object_mut() else { return };
    if let Some(Value::String(m)) = obj.get("model")
        && let Some(stripped) = m.strip_prefix("grok/")
    {
        obj.insert("model".into(), Value::String(stripped.to_string()));
    }
    // The proxy streams; translated non-streaming clients are aggregated downstream.
    obj.insert("stream".into(), Value::Bool(true));
    for k in ["previous_response_id", "prompt_cache_retention", "safety_identifier", "stream_options", "stop"] {
        obj.remove(k);
    }
    if !headers.contains_key("x-grok-conv-id")
        && let Some(key) = obj.get("prompt_cache_key").and_then(Value::as_str)
    {
        set_header(headers, "x-grok-conv-id", key);
    }
}

pub fn session_key(headers: &HeaderMap, body: &Value) -> Option<String> {
    ["x-grok-session-id", "x-grok-conv-id"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string))
        .or_else(|| body.get("prompt_cache_key").and_then(Value::as_str).map(str::to_string))
}

// ------------------------------------------------------------------ OAuth

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

/// OIDC discovery. The endpoints must stay on the issuer's scheme and on its
/// host or a sibling host under the same parent domain.
pub async fn discover(http: &reqwest::Client, ep: &Endpoints) -> anyhow::Result<Discovery> {
    let issuer = url::Url::parse(ep.grok_auth.trim_end_matches('/'))?;
    let url = format!("{}/.well-known/openid-configuration", ep.grok_auth.trim_end_matches('/'));
    let resp = http.get(&url).header("accept", "application/json").send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "xAI discovery failed: {status}: {}", text.chars().take(200).collect::<String>());
    let d: Discovery = serde_json::from_str(&text)?;
    for (field, endpoint) in [("device_authorization_endpoint", &d.device_authorization_endpoint), ("token_endpoint", &d.token_endpoint)] {
        anyhow::ensure!(trusted_endpoint(&issuer, endpoint), "xAI discovery returned an untrusted {field}: {endpoint}");
    }
    Ok(d)
}

fn trusted_endpoint(issuer: &url::Url, endpoint: &str) -> bool {
    let Ok(u) = url::Url::parse(endpoint) else { return false };
    let (Some(host), Some(issuer_host)) = (u.host_str(), issuer.host_str()) else { return false };
    if u.scheme() != issuer.scheme() {
        return false;
    }
    if host == issuer_host {
        return u.port_or_known_default() == issuer.port_or_known_default();
    }
    // auth.x.ai may hand out endpoints on other x.ai hosts.
    match issuer_host.split_once('.') {
        Some((_, parent)) if parent.contains('.') => host.ends_with(&format!(".{parent}")) || host == parent,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// Where the user goes; includes the code when the server offers that.
    pub verification_url: String,
    pub interval: u64,
    pub expires_in: u64,
    pub token_endpoint: String,
}

pub async fn start_device(http: &reqwest::Client, ep: &Endpoints) -> anyhow::Result<DeviceCode> {
    let d = discover(http, ep).await?;
    let resp = http
        .post(&d.device_authorization_endpoint)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(status.is_success(), "device code request failed: {status}: {body}");
    let s = |k: &str| body.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let n = |k: &str| body.get(k).and_then(Value::as_u64);
    Ok(DeviceCode {
        device_code: s("device_code").ok_or_else(|| anyhow::anyhow!("missing device_code"))?,
        user_code: s("user_code").ok_or_else(|| anyhow::anyhow!("missing user_code"))?,
        verification_url: s("verification_uri_complete")
            .or_else(|| s("verification_uri"))
            .ok_or_else(|| anyhow::anyhow!("missing verification_uri"))?,
        interval: n("interval").unwrap_or(5).clamp(1, 60),
        expires_in: n("expires_in").unwrap_or(900),
        token_endpoint: d.token_endpoint,
    })
}

pub enum DevicePoll {
    Pending,
    SlowDown,
    Done(TokenResponse),
}

/// One device-code poll (RFC 8628).
pub async fn poll_device(http: &reqwest::Client, dc: &DeviceCode) -> anyhow::Result<DevicePoll> {
    let resp = http
        .post(&dc.token_endpoint)
        .header("accept", "application/json")
        .form(&[("grant_type", DEVICE_GRANT), ("device_code", dc.device_code.as_str()), ("client_id", CLIENT_ID)])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => return Ok(DevicePoll::Pending),
        Some("slow_down") => return Ok(DevicePoll::SlowDown),
        Some("expired_token") => anyhow::bail!("the device code expired"),
        Some("access_denied") => anyhow::bail!("authorization was denied"),
        Some(e) => {
            let desc = body.get("error_description").and_then(Value::as_str).unwrap_or("");
            anyhow::bail!("device token error: {e} {desc}")
        }
        None => {}
    }
    anyhow::ensure!(status.is_success(), "device token poll failed: {status}: {}", text.chars().take(200).collect::<String>());
    Ok(DevicePoll::Done(serde_json::from_str(&text)?))
}

pub async fn refresh(http: &reqwest::Client, ep: &Endpoints, cred: &Credential) -> Result<TokenResponse, RefreshError> {
    let rt = cred.refresh_token.as_deref().ok_or(RefreshError::Terminal("no refresh token".into()))?;
    let d = discover(http, ep).await.map_err(|e| RefreshError::Transient(format!("{e:#}")))?;
    let resp = http
        .post(&d.token_endpoint)
        .header("accept", "application/json")
        .form(&[("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", rt)])
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    RefreshError::from_response(resp).await
}

/// Build/update a credential from a token response. Identity (`sub`, email)
/// comes from the id_token; a refresh without one keeps the previous values.
pub fn apply_tokens(cred: &mut Credential, tr: TokenResponse) {
    let t = now();
    cred.provider = SubProvider::Grok;
    cred.expires_at = Some(tr.expires_at(t));
    cred.last_refresh = Some(t);
    if let Some(rt) = tr.refresh_token.filter(|r| !r.is_empty()) {
        cred.refresh_token = Some(rt);
    }
    if let Some(scope) = tr.scope {
        cred.scope = Some(scope);
    }
    if let Some(claims) = tr.id_token.as_deref().and_then(jwt_claims) {
        if let Some(sub) = claims.get("sub").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            cred.account_id = Some(sub.to_string());
        }
        if let Some(email) = claims.get("email").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            cred.email = Some(email.to_lowercase());
        }
    }
    if let Some(id) = tr.id_token.filter(|t| !t.is_empty()) {
        cred.id_token = Some(id);
    }
    cred.access_token = tr.access_token;
}

pub fn models_request(http: &reqwest::Client, ep: &Endpoints, cred: &Credential, client_version: &str) -> reqwest::RequestBuilder {
    http.get(format!("{}/models", ep.grok_api.trim_end_matches('/')))
        .bearer_auth(&cred.access_token)
        .header("accept", "application/json")
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
        .header("x-authenticateresponse", "authenticate-response")
        .header("x-grok-client-version", client_version)
        .header("user-agent", format!("xai-grok-workspace/{client_version}"))
}

// ------------------------------------------------------------- failures

fn bad_credentials(raw: &[u8]) -> bool {
    let raw = String::from_utf8_lossy(raw).to_ascii_lowercase();
    raw.contains("bad-credentials") || raw.contains("access token could not be validated")
}

fn free_usage_exhausted(raw: &[u8]) -> bool {
    let raw = String::from_utf8_lossy(raw).to_ascii_lowercase();
    raw.contains("free-usage-exhausted") || raw.contains("included free usage")
}

/// Classify an error response from its status and raw body.
pub fn classify(status: u16, raw: &[u8], retry_after: Option<i64>) -> Failure {
    match status {
        401 => Failure::Auth,
        // The proxy answers 403 for an invalidated access token.
        403 if bad_credentials(raw) => Failure::Auth,
        426 => Failure::Fatal,
        429 if free_usage_exhausted(raw) => Failure::Exhausted { until: Some(now() + FREE_USAGE_COOLDOWN_SECS) },
        429 => match retry_after {
            Some(s) if s > 300 => Failure::Exhausted { until: Some(now() + s) },
            _ => Failure::Transient { retry_after },
        },
        500 | 502 | 503 | 504 | 520..=527 => Failure::Transient { retry_after },
        _ => Failure::Fatal,
    }
}

// ------------------------------------------------------------ keepalive

fn is_keepalive(event: &[u8]) -> bool {
    event.split(|b| *b == b'\n').any(|line| {
        let line = line.trim_ascii();
        if let Some(name) = line.strip_prefix(b"event:") {
            return name.trim_ascii() == b"keepalive";
        }
        line.strip_prefix(b"data:")
            .and_then(|d| serde_json::from_slice::<Value>(d.trim_ascii()).ok())
            .is_some_and(|v| v.get("type").and_then(Value::as_str) == Some("keepalive"))
    })
}

/// Rewrites the proxy's `keepalive` events to SSE comments, which every SSE
/// client skips, so they never reach a client or a translator as events.
/// Other events pass through unchanged once complete.
pub struct KeepaliveFilter<S> {
    inner: S,
    buf: Vec<u8>,
    /// How much of `buf` was already searched for an event end.
    scanned: usize,
    done: bool,
}

impl<S> KeepaliveFilter<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, buf: Vec::new(), scanned: 0, done: false }
    }

    /// Complete events in the buffer, with keepalives replaced.
    fn drain_events(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut start = 0;
        // An event end may straddle the previous chunk boundary.
        let mut i = self.scanned.saturating_sub(3);
        while i < self.buf.len() {
            // An event ends at a blank line (`\n\n` or `\r\n\r\n`).
            let end = if self.buf[i..].starts_with(b"\n\n") {
                Some(i + 2)
            } else if self.buf[i..].starts_with(b"\r\n\r\n") {
                Some(i + 4)
            } else {
                None
            };
            match end {
                Some(e) => {
                    let event = &self.buf[start..e];
                    out.extend_from_slice(if is_keepalive(event) { KEEPALIVE_COMMENT } else { event });
                    start = e;
                    i = e;
                }
                None => i += 1,
            }
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();
        out
    }
}

impl<S, E> Stream for KeepaliveFilter<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.done {
                return Poll::Ready(None);
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    self.buf.extend_from_slice(&chunk);
                    let out = self.drain_events();
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(Bytes::from(out))));
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    self.done = true;
                    let rest = std::mem::take(&mut self.buf);
                    if rest.is_empty() {
                        return Poll::Ready(None);
                    }
                    let rest = if is_keepalive(&rest) { KEEPALIVE_COMMENT.to_vec() } else { rest };
                    return Poll::Ready(Some(Ok(Bytes::from(rest))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[test]
    fn prepare_owns_identity_headers_and_keeps_grok_shell_version() {
        let cred = Credential { access_token: "xai-tok".into(), ..Default::default() };
        let mut h = HeaderMap::new();
        h.insert("user-agent", "grok-shell/1.0.60 (linux; x86_64)".parse().unwrap());
        h.insert("x-grok-client-version", "1.0.60".parse().unwrap());
        h.insert("x-xai-token-auth", "spoofed".parse().unwrap());
        h.insert("x-grok-session-id", "sess".parse().unwrap());
        let mut b = json!({"model": "grok/grok-build", "input": [], "previous_response_id": "r", "stop": ["x"]});
        prepare(&mut h, &mut b, &cred, "1.0.52");
        assert_eq!(h["authorization"], "Bearer xai-tok");
        assert_eq!(h["x-xai-token-auth"], "xai-grok-cli");
        assert_eq!(h["x-grok-client-version"], "1.0.60");
        assert_eq!(h["user-agent"], "grok-shell/1.0.60 (linux; x86_64)");
        assert_eq!(h["x-grok-session-id"], "sess", "conversation headers pass through");
        assert_eq!(b["model"], "grok-build");
        assert_eq!(b["stream"], true);
        assert!(b.get("previous_response_id").is_none() && b.get("stop").is_none());

        // Other clients get the configured version and can't pick their own.
        let mut h = HeaderMap::new();
        h.insert("user-agent", "codex_cli_rs/0.1".parse().unwrap());
        h.insert("x-grok-client-version", "0.0.1".parse().unwrap());
        let mut b = json!({"model": "grok-build", "prompt_cache_key": "pck"});
        prepare(&mut h, &mut b, &cred, "1.0.52");
        assert_eq!(h["x-grok-client-version"], "1.0.52");
        assert_eq!(h["user-agent"], "xai-grok-workspace/1.0.52");
        assert_eq!(h["x-grok-conv-id"], "pck");
    }

    #[test]
    fn session_prefers_session_then_conversation() {
        let mut h = HeaderMap::new();
        h.insert("x-grok-conv-id", "conv".parse().unwrap());
        assert_eq!(session_key(&h, &json!({})).as_deref(), Some("conv"));
        h.insert("x-grok-session-id", "sess".parse().unwrap());
        assert_eq!(session_key(&h, &json!({})).as_deref(), Some("sess"));
        assert_eq!(session_key(&HeaderMap::new(), &json!({"prompt_cache_key": "k"})).as_deref(), Some("k"));
    }

    #[test]
    fn classifies_failures() {
        let c = |status: u16, raw: &str| classify(status, raw.as_bytes(), None);
        assert_eq!(c(401, "{}"), Failure::Auth);
        assert_eq!(c(403, r#"{"code":"bad-credentials"}"#), Failure::Auth);
        assert_eq!(c(403, r#"{"error":"forbidden"}"#), Failure::Fatal);
        assert_eq!(c(426, "{}"), Failure::Fatal);
        assert!(matches!(c(429, r#"{"code":"subscription:free-usage-exhausted"}"#), Failure::Exhausted { until: Some(_) }));
        assert_eq!(c(429, "{}"), Failure::Transient { retry_after: None });
        assert_eq!(c(503, "{}"), Failure::Transient { retry_after: None });
        assert_eq!(c(400, "{}"), Failure::Fatal);
    }

    #[test]
    fn discovery_endpoints_stay_on_the_issuer_domain() {
        let issuer = url::Url::parse("https://auth.x.ai").unwrap();
        assert!(trusted_endpoint(&issuer, "https://auth.x.ai/oauth2/token"));
        assert!(trusted_endpoint(&issuer, "https://accounts.x.ai/device"));
        assert!(!trusted_endpoint(&issuer, "http://auth.x.ai/oauth2/token"));
        assert!(!trusted_endpoint(&issuer, "https://evil.example/token"));
        assert!(!trusted_endpoint(&issuer, "https://x.ai.evil.example/token"));
        let local = url::Url::parse("http://127.0.0.1:9000").unwrap();
        assert!(trusted_endpoint(&local, "http://127.0.0.1:9000/token"));
        assert!(!trusted_endpoint(&local, "http://127.0.0.1:9001/token"));
    }

    #[tokio::test]
    async fn keepalives_become_comments_across_chunk_boundaries() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"event: keepalive\ndata: {\"type\":\"keep")),
            Ok(Bytes::from_static(b"alive\",\"ts\":1}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n")),
            Ok(Bytes::from_static(b"\ndata: {\"type\":\"keepalive\"}\r\n\r\ndata: {\"type\":\"response.completed\"}\n\n")),
        ];
        let out: Vec<u8> =
            KeepaliveFilter::new(futures::stream::iter(chunks)).map(|c| c.unwrap().to_vec()).collect::<Vec<_>>().await.concat();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            ": keepalive\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n: keepalive\n\ndata: {\"type\":\"response.completed\"}\n\n"
        );
    }
}
