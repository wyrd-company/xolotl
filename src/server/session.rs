//! Web UI sign-in: one admin password, checked against an argon2id hash in
//! the vault `admin` secret (`ui_password_hash`), and an in-memory session
//! behind an HttpOnly cookie.
//!
//! A session authorizes only the admin API, and only together with its CSRF
//! token in `x-csrf-token` on every request. Browsers never add that header
//! to a cross-site request, so the cookie alone grants nothing.
//!
//! Sessions don't survive a restart and aren't shared between instances.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, authorize, json_error};
use crate::keys::{Principal, Scope, random_id, sha256_hex};
use crate::policy::Policy;
use crate::secrets::Conflict;

pub const COOKIE: &str = "xolotl_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Field of the vault `admin` secret that holds the password hash.
pub const HASH_FIELD: &str = "ui_password_hash";

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

struct Session {
    csrf: String,
    expires: Instant,
}

/// Sessions keyed by the SHA-256 of the cookie value, and the password
/// generation they were created under.
#[derive(Default)]
struct Live {
    /// Bumped by every password change, together with clearing `sessions`.
    generation: u64,
    sessions: HashMap<String, Session>,
}

/// Live sessions and failed sign-in attempts per client address.
#[derive(Default)]
pub struct Sessions {
    live: Mutex<Live>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Sessions {
    /// The current password generation. A sign-in reads it before reading
    /// the password hash and passes it to [`Sessions::create`].
    fn generation(&self) -> u64 {
        self.live.lock().generation
    }

    /// Start a session: (cookie value, CSRF token). `None` if the password
    /// changed since `generation` was read, so a sign-in checked against
    /// the old password can't outlive the change.
    fn create(&self, generation: u64) -> Option<(String, String)> {
        let mut live = self.live.lock();
        if live.generation != generation {
            return None;
        }
        let id = random_id(43);
        let csrf = random_id(32);
        let now = Instant::now();
        live.sessions.retain(|_, s| s.expires > now);
        live.sessions.insert(sha256_hex(&id), Session { csrf: csrf.clone(), expires: now + SESSION_TTL });
        Some((id, csrf))
    }

    /// The CSRF token of the session named by the request's cookie.
    fn csrf(&self, headers: &HeaderMap) -> Option<String> {
        let id = cookie(headers, COOKIE)?;
        let live = self.live.lock();
        live.sessions.get(&sha256_hex(&id)).filter(|s| s.expires > Instant::now()).map(|s| s.csrf.clone())
    }

    /// The admin principal for a request with a live session cookie and its CSRF token.
    pub fn principal(&self, headers: &HeaderMap) -> Option<Principal> {
        let expected = self.csrf(headers)?;
        let presented = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok())?;
        crate::keys::ct_eq(expected.as_bytes(), presented.as_bytes())
            .then(|| Principal { client_id: None, policy: Arc::new(Policy::unrestricted()) })
    }

    fn remove(&self, headers: &HeaderMap) {
        if let Some(id) = cookie(headers, COOKIE) {
            self.live.lock().sessions.remove(&sha256_hex(&id));
        }
    }

    /// The password changed: end every session, and refuse sessions from
    /// sign-ins that started before.
    fn password_changed(&self) {
        let mut live = self.live.lock();
        live.generation += 1;
        live.sessions.clear();
    }

    /// Seconds left in a lockout of `ip`, if any.
    fn locked(&self, ip: IpAddr) -> Option<u64> {
        let mut failures = self.failures.lock();
        let (count, since) = *failures.get(&ip)?;
        let elapsed = since.elapsed();
        if elapsed >= LOCKOUT {
            failures.remove(&ip);
            return None;
        }
        (count >= MAX_FAILURES).then(|| (LOCKOUT - elapsed).as_secs().max(1))
    }

    fn fail(&self, ip: IpAddr) {
        let mut failures = self.failures.lock();
        let e = failures.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= LOCKOUT {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    fn succeed(&self, ip: IpAddr) {
        self.failures.lock().remove(&ip);
    }
}

/// A cookie's value from the request's `Cookie` headers.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

/// `Secure` when the browser reached us over HTTPS (through a TLS-terminating proxy).
fn set_cookie(headers: &HeaderMap, value: &str, max_age: u64) -> HeaderValue {
    let https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"))
        .expect("cookie value is ASCII")
}

/// Hash a password into an argon2id PHC string.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string()).map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Accept only argon2id PHC strings.
fn valid_hash(phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| h.algorithm == Algorithm::Argon2id.ident())
}

fn verify(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

pub fn router() -> Router<AppState> {
    Router::new().route("/session", get(status).post(login).delete(logout)).route("/ui/password", put(set_password))
}

async fn stored_hash(s: &AppState) -> Result<Option<String>, Response> {
    let admin = s.secrets.get("admin").await.map_err(|e| json_error(StatusCode::SERVICE_UNAVAILABLE, format!("vault: {e:#}")))?;
    Ok(admin.and_then(|m| m.get(HASH_FIELD).and_then(Value::as_str).map(str::to_string)).filter(|h| !h.is_empty()))
}

/// Whether the caller is signed in (with the CSRF token to use), and whether
/// a password is set at all.
async fn status(State(s): State<AppState>, h: HeaderMap) -> Response {
    if let Some(csrf) = s.sessions.csrf(&h) {
        return Json(json!({"authenticated": true, "csrf_token": csrf})).into_response();
    }
    match stored_hash(&s).await {
        Ok(hash) => Json(json!({"authenticated": false, "password_set": hash.is_some()})).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(State(s): State<AppState>, req: Request) -> Response {
    // Without connection info (tests, unusual servers) all callers share one bucket.
    let ip = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let headers = req.headers().clone();
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return json_error(StatusCode::BAD_REQUEST, "request body too large");
    };
    let Ok(body) = serde_json::from_slice::<LoginReq>(&bytes) else {
        return json_error(StatusCode::BAD_REQUEST, "expected {\"password\": ...}");
    };
    if let Some(secs) = s.sessions.locked(ip) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, format!("too many failed attempts; try again in {} min", secs.div_ceil(60)));
    }
    // Before reading the hash, so a password change after this point voids the sign-in.
    let generation = s.sessions.generation();
    let hash = match stored_hash(&s).await {
        Ok(Some(h)) => h,
        Ok(None) => return json_error(StatusCode::CONFLICT, "no UI password is set; run `xolotl ui set-password`"),
        Err(r) => return r,
    };
    let ok = tokio::task::spawn_blocking(move || verify(&body.password, &hash)).await.unwrap_or(false);
    if !ok {
        s.sessions.fail(ip);
        tracing::warn!(%ip, "failed UI sign-in");
        return json_error(StatusCode::UNAUTHORIZED, "wrong password");
    }
    s.sessions.succeed(ip);
    let Some((id, csrf)) = s.sessions.create(generation) else {
        return json_error(StatusCode::CONFLICT, "the password changed while signing in; sign in again");
    };
    tracing::info!(%ip, "UI sign-in");
    let mut resp = Json(json!({"authenticated": true, "csrf_token": csrf})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie(&headers, &id, SESSION_TTL.as_secs()));
    resp
}

async fn logout(State(s): State<AppState>, h: HeaderMap) -> Response {
    if s.sessions.principal(&h).is_none() {
        return json_error(StatusCode::UNAUTHORIZED, "not signed in");
    }
    s.sessions.remove(&h);
    let mut resp = Json(json!({"authenticated": false})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie(&h, "", 0));
    resp
}

#[derive(Deserialize)]
struct PasswordReq {
    /// An argon2id PHC string; the CLI hashes locally so the password never
    /// leaves the operator's machine.
    password_hash: String,
}

/// Store the UI password hash next to the bootstrap token, keeping the
/// secret's other fields, and end every session.
async fn set_password(State(s): State<AppState>, h: HeaderMap, Json(req): Json<PasswordReq>) -> Response {
    if let Err(r) = authorize(&s, &h, Scope::Admin).await {
        return r;
    }
    if !valid_hash(&req.password_hash) {
        return json_error(StatusCode::BAD_REQUEST, "password_hash must be an argon2id PHC string");
    }
    for _ in 0..3 {
        let (data, version) = match s.secrets.get_versioned("admin").await {
            Ok(x) => x,
            Err(e) => return json_error(StatusCode::SERVICE_UNAVAILABLE, format!("vault: {e:#}")),
        };
        let mut data: Map<String, Value> = data.unwrap_or_default();
        data.insert(HASH_FIELD.into(), Value::String(req.password_hash.clone()));
        match s.secrets.put_cas("admin", data, version).await {
            Ok(_) => {
                s.sessions.password_changed();
                tracing::info!("UI password changed");
                return Json(json!({"stored": true})).into_response();
            }
            Err(e) if e.is::<Conflict>() => continue,
            Err(e) => return json_error(StatusCode::SERVICE_UNAVAILABLE, format!("vault: {e:#}")),
        }
    }
    json_error(StatusCode::CONFLICT, "the admin secret kept changing; try again")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_only_argon2id_is_accepted() {
        let h = hash_password("hunter42").unwrap();
        assert!(valid_hash(&h));
        assert!(verify("hunter42", &h));
        assert!(!verify("hunter43", &h));
        assert!(!valid_hash("$argon2i$v=19$m=16,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2g"));
        assert!(!valid_hash("plaintext"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1; xolotl_session=abc"));
        assert_eq!(cookie(&h, COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "b"), None);
    }

    #[test]
    fn password_change_voids_sessions_and_sign_ins_in_flight() {
        let s = Sessions::default();
        let (id, csrf) = s.create(s.generation()).unwrap();
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(&format!("{COOKIE}={id}")).unwrap());
        h.insert(CSRF_HEADER, HeaderValue::from_str(&csrf).unwrap());
        assert!(s.principal(&h).is_some());

        // A sign-in read the generation (and then the old hash); the password changes before it finishes.
        let before = s.generation();
        s.password_changed();
        assert!(s.principal(&h).is_none(), "existing sessions end");
        assert!(s.create(before).is_none(), "the in-flight sign-in gets no session");
        assert!(s.create(s.generation()).is_some(), "a sign-in after the change works");
    }

    #[test]
    fn lockout_after_repeated_failures() {
        let s = Sessions::default();
        let ip = IpAddr::from([10, 0, 0, 1]);
        for _ in 0..MAX_FAILURES - 1 {
            s.fail(ip);
        }
        assert!(s.locked(ip).is_none());
        s.fail(ip);
        assert!(s.locked(ip).is_some());
        assert!(s.locked(IpAddr::from([10, 0, 0, 2])).is_none());
    }
}
