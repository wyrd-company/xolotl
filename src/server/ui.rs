//! The admin web UI's static files: `ui/dist`, built by `pnpm build` in `ui/`.
//!
//! Release builds embed the files; debug builds read them from disk, so a
//! rebuilt UI shows up without recompiling. The app lives at `/` and routes
//! with the URL fragment (`/#/accounts`), so it claims only `/` and `/assets/`.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist"]
#[allow_missing = true]
struct Dist;

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

const NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>xolotl</title>\
    <p>The admin UI was not built into this binary. Run <code>pnpm install &amp;&amp; pnpm build</code> in <code>ui/</code>, then rebuild xolotl.</p>";

pub async fn index() -> Response {
    let mut resp = match Dist::get("index.html") {
        Some(f) => Html(f.data.into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, Html(NOT_BUILT)).into_response(),
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

/// Vite names every file under `assets/` by its content hash, so they never change.
pub async fn asset(Path(path): Path<String>) -> Response {
    if path.split('/').any(|seg| seg == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(f) = Dist::get(&format!("assets/{path}")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = HeaderValue::from_str(f.metadata.mimetype()).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable")),
            (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ],
        f.data.into_owned(),
    )
        .into_response()
}
