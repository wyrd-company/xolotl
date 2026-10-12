//! Minimal OpenBao / Vault client: AppRole login with automatic re-login and
//! the KV v2 operations xolotl needs. Implemented directly on reqwest so the
//! surface stays small and OpenBao-compatible.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;

use crate::config::VaultConfig;
use crate::secrets::SecretStore;

#[derive(Clone)]
pub struct VaultClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    address: String,
    namespace: Option<String>,
    kv_mount: String,
    prefix: String,
    approle_mount: String,
    role_id: String,
    secret_id: String,
    token: RwLock<Option<Token>>,
}

#[derive(Clone)]
struct Token {
    value: String,
    /// When to proactively log in again (2/3 of the lease).
    refresh_at: Option<Instant>,
}

#[derive(Deserialize)]
struct LoginResponse {
    auth: LoginAuth,
}

#[derive(Deserialize)]
struct LoginAuth {
    client_token: String,
    #[serde(default)]
    lease_duration: u64,
}

impl VaultClient {
    pub fn new(cfg: &VaultConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(15));
        if let Some(ca) = &cfg.ca_cert {
            let pem = std::fs::read(ca).with_context(|| format!("reading vault CA {}", ca.display()))?;
            for cert in reqwest::Certificate::from_pem_bundle(&pem).context("parsing vault CA bundle")? {
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                http: builder.build()?,
                address: cfg.address.trim_end_matches('/').to_string(),
                namespace: cfg.namespace.clone(),
                kv_mount: cfg.mount.trim_matches('/').to_string(),
                prefix: cfg.prefix.trim_matches('/').to_string(),
                approle_mount: cfg.approle.mount.trim_matches('/').to_string(),
                role_id: cfg.approle.resolve_role_id()?,
                secret_id: cfg.approle.resolve_secret_id()?,
                token: RwLock::new(None),
            }),
        })
    }

    async fn login(&self) -> Result<Token> {
        let url = format!("{}/v1/auth/{}/login", self.inner.address, self.inner.approle_mount);
        let mut req = self.inner.http.post(&url).json(&json!({"role_id": self.inner.role_id, "secret_id": self.inner.secret_id}));
        if let Some(ns) = &self.inner.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        let resp = req.send().await.context("vault approle login")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("vault approle login failed: {status}: {}", truncate(&body, 300));
        }
        let parsed: LoginResponse = resp.json().await.context("decoding vault login response")?;
        let refresh_at = (parsed.auth.lease_duration > 0).then(|| Instant::now() + Duration::from_secs(parsed.auth.lease_duration * 2 / 3));
        tracing::debug!(lease = parsed.auth.lease_duration, "vault approle login ok");
        Ok(Token { value: parsed.auth.client_token, refresh_at })
    }

    async fn token(&self, force: bool) -> Result<String> {
        if !force {
            let guard = self.inner.token.read().await;
            if let Some(t) = guard.as_ref()
                && t.refresh_at.is_none_or(|at| Instant::now() < at)
            {
                return Ok(t.value.clone());
            }
        }
        let mut guard = self.inner.token.write().await;
        // Another task may have logged in while we waited for the lock.
        if !force
            && let Some(t) = guard.as_ref()
            && t.refresh_at.is_none_or(|at| Instant::now() < at)
        {
            return Ok(t.value.clone());
        }
        let t = self.login().await?;
        let value = t.value.clone();
        *guard = Some(t);
        Ok(value)
    }

    /// Perform a request, logging in again once on 403 (expired/revoked token).
    async fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<(StatusCode, Value)> {
        let url = format!("{}/v1/{}", self.inner.address, path);
        for attempt in 0..2 {
            let token = self.token(attempt > 0).await?;
            let mut req = self.inner.http.request(method.clone(), &url).header("X-Vault-Token", token);
            if let Some(ns) = &self.inner.namespace {
                req = req.header("X-Vault-Namespace", ns);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.with_context(|| format!("vault {method} {path}"))?;
            let status = resp.status();
            if status == StatusCode::FORBIDDEN && attempt == 0 {
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            let value = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            return Ok((status, value));
        }
        unreachable!()
    }

    fn full_path(&self, key: &str) -> String {
        let key = key.trim_matches('/');
        if self.inner.prefix.is_empty() { key.to_string() } else { format!("{}/{}", self.inner.prefix, key) }
    }

    pub async fn health(&self) -> Result<()> {
        self.token(false).await.map(|_| ())
    }
}

#[async_trait::async_trait]
impl SecretStore for VaultClient {
    async fn get(&self, key: &str) -> Result<Option<Map<String, Value>>> {
        let path = format!("{}/data/{}", self.inner.kv_mount, self.full_path(key));
        let (status, body) = self.request(Method::GET, &path, None).await?;
        match status {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => match body.pointer("/data/data") {
                Some(Value::Object(m)) => Ok(Some(m.clone())),
                // Soft-deleted latest version: data is null.
                _ => Ok(None),
            },
            s => Err(anyhow!("vault read {key}: {s}: {}", errors(&body))),
        }
    }

    async fn put(&self, key: &str, data: Map<String, Value>) -> Result<()> {
        let path = format!("{}/data/{}", self.inner.kv_mount, self.full_path(key));
        let (status, body) = self.request(Method::POST, &path, Some(&json!({ "data": data }))).await?;
        if !status.is_success() {
            anyhow::bail!("vault write {key}: {status}: {}", errors(&body));
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // Deleting metadata removes every version, so revoked material is gone.
        let path = format!("{}/metadata/{}", self.inner.kv_mount, self.full_path(key));
        let (status, body) = self.request(Method::DELETE, &path, None).await?;
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            anyhow::bail!("vault delete {key}: {status}: {}", errors(&body));
        }
        Ok(())
    }

    async fn list(&self, dir: &str) -> Result<Vec<String>> {
        let path = format!("{}/metadata/{}", self.inner.kv_mount, self.full_path(dir));
        let method = Method::from_bytes(b"LIST").expect("valid method");
        let (status, body) = self.request(method, &path, None).await?;
        match status {
            StatusCode::NOT_FOUND => Ok(Vec::new()),
            s if s.is_success() => Ok(body
                .pointer("/data/keys")
                .and_then(Value::as_array)
                .map(|keys| keys.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())
                .unwrap_or_default()),
            s => Err(anyhow!("vault list {dir}: {s}: {}", errors(&body))),
        }
    }
}

fn errors(body: &Value) -> String {
    body.get("errors").map(|e| e.to_string()).unwrap_or_else(|| "no error detail".into())
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(addr: &str) -> VaultConfig {
        let mut c = VaultConfig { address: addr.into(), ..Default::default() };
        c.approle.role_id = Some("role".into());
        c.approle.secret_id = Some("secret".into());
        c
    }

    #[tokio::test]
    async fn approle_login_then_kv_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "auth": {"client_token": "tok", "lease_duration": 3600}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/xolotl/providers/vercel"))
            .and(header("X-Vault-Token", "tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"data": {"api_key": "k"}, "metadata": {}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/xolotl/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errors": []})))
            .mount(&server)
            .await;
        Mock::given(method("LIST"))
            .and(path("/v1/secret/metadata/xolotl/clients"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"keys": ["a", "b"]}})))
            .mount(&server)
            .await;

        let v = VaultClient::new(&cfg(&server.uri())).unwrap();
        let got = v.get("providers/vercel").await.unwrap().unwrap();
        assert_eq!(got["api_key"], "k");
        assert!(v.get("missing").await.unwrap().is_none());
        assert_eq!(v.list("clients").await.unwrap(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn relogs_in_on_forbidden() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/approle/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "auth": {"client_token": "tok", "lease_duration": 0}
            })))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/xolotl/x"))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/xolotl/x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"data": {"a": 1}}})))
            .mount(&server)
            .await;
        let v = VaultClient::new(&cfg(&server.uri())).unwrap();
        assert_eq!(v.get("x").await.unwrap().unwrap()["a"], 1);
    }
}
