//! Command-line interface. `serve` runs the server; every other command talks
//! to a running server's admin API (`XOLOTL_URL`, `XOLOTL_ADMIN_TOKEN`).

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use crate::catalog::ModelCatalog;
use crate::config::Config;
use crate::db::Db;
use crate::gateway::Gateway;
use crate::keys::KeyManager;
use crate::secrets::Secrets;
use crate::server::{self, AppState};
use crate::subscription::{SubProvider, Subscriptions};
use crate::telemetry;
use crate::vault::VaultClient;

#[derive(Parser)]
#[command(name = "xolotl", version, about = "LLM subscription proxy and AI gateway backed by OpenBao")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Clone)]
pub struct Remote {
    /// Server base URL.
    #[arg(long, env = "XOLOTL_URL", default_value = "http://127.0.0.1:8317", global = true)]
    url: String,
    /// Admin token (the vault `admin` secret's token, or an admin-scoped key).
    #[arg(long, env = "XOLOTL_ADMIN_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the proxy server.
    Serve {
        #[arg(long, short, env = "XOLOTL_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Validate a configuration file and print it with credentials redacted.
    CheckConfig {
        #[arg(long, short, env = "XOLOTL_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Manage downstream client keys.
    Keys {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    /// Manage client key policies (provider/model access, precedence, aliases).
    Policies {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: PoliciesCmd,
    },
    /// Show the model catalog discovered from all providers.
    Models {
        #[command(flatten)]
        remote: Remote,
        /// Re-discover before printing.
        #[arg(long)]
        refresh: bool,
    },
    /// Manage subscription accounts.
    Accounts {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    /// Log in subscription accounts.
    Auth {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Manage gateway provider credentials.
    Providers {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: ProvidersCmd,
    },
    /// Inspect the MCP gateway's upstream servers and set their credentials.
    Mcp {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Show request/token usage from the request log.
    Usage {
        #[command(flatten)]
        remote: Remote,
        /// client, account, provider or model.
        #[arg(long, default_value = "client")]
        group_by: String,
        /// Look-back window, e.g. 1h, 24h, 7d.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Manage the admin web UI.
    Ui {
        #[command(flatten)]
        remote: Remote,
        #[command(subcommand)]
        cmd: UiCmd,
    },
    /// Reload keys and accounts from the vault.
    Reload {
        #[command(flatten)]
        remote: Remote,
    },
}

#[derive(Subcommand)]
pub enum KeysCmd {
    /// Issue a key into the vault at keys/<policy>/<name>. The key is never
    /// printed; the client reads it from the vault with its own Vault policy.
    Issue {
        #[arg(long)]
        policy: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Expire after this long, e.g. 30d.
        #[arg(long)]
        ttl: Option<String>,
    },
    List,
    /// Write a new key for `<policy>/<name>`.
    Rotate {
        id: String,
        /// Keep the old key valid this long, e.g. 1h.
        #[arg(long)]
        grace: Option<String>,
    },
    /// Revoke `<policy>/<name>`: writes a marker and destroys old versions.
    Revoke {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
}

#[derive(Subcommand)]
pub enum PoliciesCmd {
    List,
    Get {
        name: String,
    },
    /// Create or replace a policy from a JSON or TOML file.
    Put {
        file: PathBuf,
    },
    Delete {
        name: String,
    },
    /// Show the model names the policy exposes and where they route.
    View {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum AccountsCmd {
    List {
        #[arg(long, value_enum)]
        provider: Option<SubProvider>,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Clear cooldown and auth-failure state.
    Reset {
        id: String,
    },
    Remove {
        id: String,
    },
    /// Higher priority accounts are used first.
    Priority {
        id: String,
        priority: i64,
    },
    /// Fetch usage from the provider now.
    Usage {
        id: String,
    },
    /// Import `~/.codex/auth.json` or `~/.claude/.credentials.json`. The local
    /// CLI should stop using that login afterwards (refresh tokens rotate).
    Import {
        #[arg(value_enum)]
        provider: SubProvider,
        file: PathBuf,
        #[arg(long)]
        label: Option<String>,
    },
    /// Add an API-key account (OpenCode Go). Reads the key from stdin.
    AddKey {
        #[arg(long, value_enum, default_value = "opencode-go")]
        provider: SubProvider,
        #[arg(long)]
        label: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AuthCmd {
    /// Log in a Codex (ChatGPT), Claude or Grok Build (xAI) subscription account.
    Login {
        #[arg(value_enum)]
        provider: SubProvider,
        /// Codex: use the browser/paste flow instead of the device code.
        #[arg(long)]
        browser: bool,
    },
}

#[derive(Subcommand)]
pub enum ProvidersCmd {
    /// Store a gateway provider API key in the vault. Reads the key from stdin.
    SetKey { name: String },
}

#[derive(Subcommand)]
pub enum McpCmd {
    /// List upstream MCP servers: connection, tool count, secret fields.
    Servers,
    /// List one server's tools (unfiltered by any policy).
    Tools { server: String },
    /// Re-read tool lists from every server, or from one.
    Refresh { server: Option<String> },
    /// Store a field of a server's vault secret (`mcp/<server>`), used by its
    /// `{field}` header placeholders. Reads the value from stdin.
    SetSecret { server: String, field: String },
    /// Remove a field of a server's vault secret.
    UnsetSecret { server: String, field: String },
    /// Sign in to a server that uses OAuth (`auth = "oauth"`): open the URL
    /// printed, approve, then paste the URL the browser ends on (unless
    /// `server.public_url` lets xolotl finish the login itself).
    Login { server: String },
    /// Forget a server's OAuth tokens.
    Logout { server: String },
    /// Show MCP tool calls from the call log.
    Usage {
        /// client, server or tool.
        #[arg(long, default_value = "server")]
        group_by: String,
        /// Look-back window, e.g. 1h, 24h, 7d.
        #[arg(long, default_value = "24h")]
        since: String,
    },
}

#[derive(Subcommand)]
pub enum UiCmd {
    /// Set the web UI password. It is hashed (argon2id) here and only the hash
    /// is stored, in the vault `admin` secret. Signs out every UI session.
    /// Reads the password from stdin when it is not a terminal.
    SetPassword,
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve { config } => serve(config).await,
        Command::CheckConfig { config } => {
            let cfg = Config::load(config.as_deref())?;
            println!("{}", toml::to_string_pretty(&cfg.redacted())?);
            Ok(())
        }
        Command::Keys { remote, cmd } => keys(Client::new(&remote)?, cmd).await,
        Command::Policies { remote, cmd } => policies(Client::new(&remote)?, cmd).await,
        Command::Models { remote, refresh } => {
            let c = Client::new(&remote)?;
            let v = if refresh { c.post("/admin/catalog/refresh", json!({})).await? } else { c.get("/admin/catalog").await? };
            if c.json {
                return c.print(v);
            }
            for (provider, models) in v.as_object().into_iter().flatten() {
                let list: Vec<&str> = models.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                println!("{provider} ({}):", list.len());
                for m in list {
                    println!("  {m}");
                }
            }
            Ok(())
        }
        Command::Accounts { remote, cmd } => accounts(Client::new(&remote)?, cmd).await,
        Command::Auth { remote, cmd } => auth(Client::new(&remote)?, cmd).await,
        Command::Providers { remote, cmd } => {
            let c = Client::new(&remote)?;
            match cmd {
                ProvidersCmd::SetKey { name } => {
                    let key = read_secret(&format!("API key for {name}: "))?;
                    c.print(c.post(&format!("/admin/providers/{name}/key"), json!({"api_key": key})).await?)
                }
            }
        }
        Command::Mcp { remote, cmd } => mcp(Client::new(&remote)?, cmd).await,
        Command::Usage { remote, group_by, since } => {
            let c = Client::new(&remote)?;
            let secs = parse_duration(&since)?;
            let v = c.get(&format!("/admin/usage?group_by={group_by}&since_secs={secs}")).await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<40} {:>8} {:>7} {:>12} {:>12} {:>12}", group_by.to_uppercase(), "REQS", "ERRORS", "INPUT", "OUTPUT", "CACHED");
            for r in v.as_array().into_iter().flatten() {
                println!(
                    "{:<40} {:>8} {:>7} {:>12} {:>12} {:>12}",
                    r["key"].as_str().unwrap_or("-"),
                    r["requests"].as_i64().unwrap_or(0),
                    r["errors"].as_i64().unwrap_or(0),
                    r["input_tokens"].as_i64().unwrap_or(0),
                    r["output_tokens"].as_i64().unwrap_or(0),
                    r["cached_tokens"].as_i64().unwrap_or(0)
                );
            }
            Ok(())
        }
        Command::Ui { remote, cmd: UiCmd::SetPassword } => {
            let c = Client::new(&remote)?;
            let password = read_new_password()?;
            let hash = server::session::hash_password(&password)?;
            c.print(c.put("/admin/ui/password", json!({"password_hash": hash})).await?)
        }
        Command::Reload { remote } => {
            let c = Client::new(&remote)?;
            c.print(c.post("/admin/reload", json!({})).await?)
        }
    }
}

async fn serve(config: Option<PathBuf>) -> Result<()> {
    let cfg = Config::load(config.as_deref())?;
    let vault = VaultClient::new(&cfg.vault)?;
    let secrets: Secrets = Arc::new(vault.clone());

    // OTLP headers (e.g. OpenObserve basic auth) may live in the vault.
    let extra_headers = match secrets.get("telemetry").await {
        Ok(Some(m)) => m.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect(),
        Ok(None) => Default::default(),
        Err(e) => {
            eprintln!("warning: could not read telemetry secret from vault: {e:#}");
            Default::default()
        }
    };
    let guard = telemetry::init(&cfg.telemetry, extra_headers)?;

    let db = Db::open(&cfg.database.path).await?;
    let http = crate::proxy::upstream_client(Duration::from_secs(cfg.server.upstream_timeout_secs))?;

    let keys = Arc::new(KeyManager::new(secrets.clone()));
    let (policies, n) = keys.reload().await.context("loading policies and client keys from the vault")?;
    tracing::info!(policies, keys = n, "loaded policies and client keys");
    let subs = Arc::new(
        Subscriptions::new(cfg.subscription.clone(), db.clone(), secrets.clone(), http.clone())
            .with_grok_client_version(cfg.catalog.grok_client_version.clone()),
    );
    let n = subs.sync().await.context("loading accounts from the vault")?;
    tracing::info!(accounts = n, "loaded subscription accounts");
    let gateway = Arc::new(Gateway::new(&cfg.gateway, secrets.clone(), http, db.clone()));
    let catalog = Arc::new(ModelCatalog::new(cfg.catalog.clone(), gateway.kinds()));

    let mcp = Arc::new(crate::mcp::McpHub::new(&cfg, secrets.clone(), db.clone()));
    let state =
        AppState { cfg: Arc::new(cfg), db, secrets, keys, subs, gateway, catalog, vault: Some(vault), sessions: Default::default(), mcp };
    server::spawn_background(state.clone());
    let subs = state.subs.clone();
    let result = server::serve(state).await;
    // Last chance to save refreshed credentials whose vault write failed.
    if subs.unsaved_count() > 0 {
        let left = subs.flush_unsaved().await;
        if left > 0 {
            tracing::error!(
                accounts = left,
                "shutting down with refreshed credentials not saved to the vault; those accounts will need to log in again"
            );
        }
    }
    guard.shutdown();
    result
}

fn key_path(id: &str) -> Result<String> {
    let (policy, name) = id.split_once('/').context("key id must be <policy>/<name>")?;
    Ok(format!("/admin/keys/{policy}/{name}"))
}

async fn keys(c: Client, cmd: KeysCmd) -> Result<()> {
    match cmd {
        KeysCmd::Issue { policy, name, description, ttl } => {
            let ttl_secs = ttl.as_deref().map(parse_duration).transpose()?;
            let v =
                c.post("/admin/keys", json!({"policy": policy, "name": name, "description": description, "ttl_secs": ttl_secs})).await?;
            if c.json {
                return c.print(v);
            }
            println!("issued {}", v["client_id"].as_str().unwrap_or(""));
            println!("vault:  <mount>/<prefix>/{}  (field api_key)", v["vault_path"].as_str().unwrap_or(""));
            Ok(())
        }
        KeysCmd::List => {
            let v = c.get("/admin/keys").await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<36} {:<16} {:<9} {:<17} {:<17} DESCRIPTION", "CLIENT", "POLICY", "STATE", "ROTATED", "EXPIRES");
            for k in v.as_array().into_iter().flatten() {
                println!(
                    "{:<36} {:<16} {:<9} {:<17} {:<17} {}",
                    k["client_id"].as_str().unwrap_or(""),
                    k["policy"].as_str().unwrap_or(""),
                    if k["disabled"].as_bool().unwrap_or(false) { "disabled" } else { "active" },
                    fmt_rfc3339(k.get("rotated_at").filter(|v| !v.is_null()).or(k.get("created_at"))),
                    fmt_rfc3339(k.get("expires_at")),
                    k["description"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        KeysCmd::Rotate { id, grace } => {
            let grace_secs = grace.as_deref().map(parse_duration).transpose()?.unwrap_or(0);
            c.print(c.post(&format!("{}/rotate", key_path(&id)?), json!({"grace_secs": grace_secs})).await?)
        }
        KeysCmd::Revoke { id } => c.print(c.delete(&key_path(&id)?).await?),
        KeysCmd::Enable { id } => c.print(c.post(&format!("{}/enable", key_path(&id)?), json!({})).await?),
        KeysCmd::Disable { id } => c.print(c.post(&format!("{}/disable", key_path(&id)?), json!({})).await?),
    }
}

async fn policies(c: Client, cmd: PoliciesCmd) -> Result<()> {
    match cmd {
        PoliciesCmd::List => {
            let v = c.get("/admin/policies").await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<20} {:<36} {:<24} DESCRIPTION", "NAME", "PROVIDERS", "SCOPES");
            for p in v.as_array().into_iter().flatten() {
                let join =
                    |k: &str| p[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")).unwrap_or_default();
                println!(
                    "{:<20} {:<36} {:<24} {}",
                    p["name"].as_str().unwrap_or(""),
                    join("providers"),
                    join("scopes"),
                    p["description"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        PoliciesCmd::Get { name } => c.print(c.get(&format!("/admin/policies/{name}")).await?),
        PoliciesCmd::Put { file } => {
            let raw = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let policy: Value = if file.extension().is_some_and(|e| e == "toml") {
                serde_json::to_value(toml::from_str::<toml::Value>(&raw)?)?
            } else {
                serde_json::from_str(&raw)?
            };
            let name = policy["name"].as_str().context("policy needs a name")?.to_string();
            c.print(c.put(&format!("/admin/policies/{name}"), policy).await?)
        }
        PoliciesCmd::Delete { name } => c.print(c.delete(&format!("/admin/policies/{name}")).await?),
        PoliciesCmd::View { name } => {
            let v = c.get(&format!("/admin/policies/{name}/view")).await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<44} {:<56} {:<36} TRANSLATED", "MODEL", "ROUTES TO", "NATIVE");
            for m in v["models"].as_array().into_iter().flatten() {
                let list = |k: &str| {
                    m[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default()
                };
                let name = if m["alias"].as_bool().unwrap_or(false) {
                    format!("{} (alias)", m["name"].as_str().unwrap_or(""))
                } else {
                    m["name"].as_str().unwrap_or("").to_string()
                };
                println!("{:<44} {:<56} {:<36} {}", name, list("targets"), list("endpoints"), list("translated_endpoints"));
            }
            let tools: Vec<&str> = v["tools"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if v["mcp_servers"].as_array().is_some_and(|a| !a.is_empty()) {
                println!("\nMCP TOOLS ({})", tools.len());
                for t in tools {
                    println!("  {t}");
                }
            }
            Ok(())
        }
    }
}

async fn mcp(c: Client, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Servers => {
            let v = c.get("/admin/mcp/servers").await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<20} {:<20} {:<10} {:>6} {:<24} URL", "NAME", "NAMESPACE", "STATE", "TOOLS", "SECRET FIELDS");
            for s in v.as_array().into_iter().flatten() {
                let fields: Vec<String> = s["secret_fields"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, set)| if set.as_bool() == Some(true) { k.clone() } else { format!("{k} (unset)") })
                    .collect();
                let state = match (s["connected"].as_bool(), s["last_error"].is_string(), s["oauth"]["needs_login"].as_str()) {
                    (_, _, Some(_)) => "SIGN-IN",
                    (_, true, _) => "ERROR",
                    (Some(true), _, _) => "connected",
                    _ => "idle",
                };
                println!(
                    "{:<20} {:<20} {:<10} {:>6} {:<24} {}",
                    s["name"].as_str().unwrap_or(""),
                    s["namespace"].as_str().unwrap_or(""),
                    state,
                    s["tools"].as_u64().unwrap_or(0),
                    if fields.is_empty() { "-".to_string() } else { fields.join(",") },
                    s["url"].as_str().unwrap_or("")
                );
                if let Some(reason) = s["oauth"]["needs_login"].as_str() {
                    println!("    OAuth: {reason}; run `xolotl mcp login {}`", s["name"].as_str().unwrap_or(""));
                } else if let Some(e) = s["last_error"].as_str() {
                    println!("    {e}");
                }
            }
            Ok(())
        }
        McpCmd::Tools { server } => {
            let v = c.get(&format!("/admin/mcp/servers/{server}/tools")).await?;
            if c.json {
                return c.print(v);
            }
            for t in v.as_array().into_iter().flatten() {
                let desc = t["description"].as_str().unwrap_or("").lines().next().unwrap_or("");
                println!("{:<40} {desc}", t["name"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        McpCmd::Refresh { server: Some(server) } => c.print(c.post(&format!("/admin/mcp/servers/{server}/refresh"), json!({})).await?),
        McpCmd::Refresh { server: None } => c.print(c.post("/admin/mcp/refresh", json!({})).await?),
        McpCmd::SetSecret { server, field } => {
            let value = read_secret(&format!("{field} for {server}: "))?;
            c.print(c.put(&format!("/admin/mcp/servers/{server}/secret"), json!({"fields": {field: value}})).await?)
        }
        McpCmd::UnsetSecret { server, field } => {
            c.print(c.put(&format!("/admin/mcp/servers/{server}/secret"), json!({"fields": {field: null}})).await?)
        }
        McpCmd::Login { server } => {
            let start = c.post(&format!("/admin/mcp/servers/{server}/oauth/start"), json!({})).await?;
            let flow_id = start["flow_id"].as_str().context("server returned no flow id")?.to_string();
            eprintln!("\nOpen this URL in a browser and approve access for xolotl:\n\n  {}\n", start["url"].as_str().unwrap_or(""));
            let done = if start["callback"].as_bool().unwrap_or(false) {
                eprintln!("Waiting for the browser to return to xolotl...");
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let s = c.get(&format!("/admin/mcp/oauth/flows/{flow_id}")).await?;
                    match s["status"].as_str() {
                        Some("pending") => continue,
                        Some("complete") => break s,
                        _ => anyhow::bail!("login failed: {}", s["error"].as_str().unwrap_or("unknown error")),
                    }
                }
            } else {
                eprintln!("The browser then goes to {} and shows an error page;", start["redirect_uri"].as_str().unwrap_or("a local URL"));
                eprint!("paste that page's full URL here: ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                c.post(&format!("/admin/mcp/oauth/flows/{flow_id}/complete"), json!({"redirect_url": line.trim()})).await?
            };
            if c.json {
                return c.print(done);
            }
            println!("Signed in to {server}");
            Ok(())
        }
        McpCmd::Logout { server } => c.print(c.delete(&format!("/admin/mcp/servers/{server}/oauth")).await?),
        McpCmd::Usage { group_by, since } => {
            let secs = parse_duration(&since)?;
            let v = c.get(&format!("/admin/mcp/usage?group_by={group_by}&since_secs={secs}")).await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<50} {:>8} {:>7} {:>10}", group_by.to_uppercase(), "CALLS", "ERRORS", "AVG MS");
            for r in v.as_array().into_iter().flatten() {
                println!(
                    "{:<50} {:>8} {:>7} {:>10.0}",
                    r["key"].as_str().unwrap_or("-"),
                    r["calls"].as_i64().unwrap_or(0),
                    r["errors"].as_i64().unwrap_or(0),
                    r["avg_latency_ms"].as_f64().unwrap_or(0.0)
                );
            }
            Ok(())
        }
    }
}

async fn accounts(c: Client, cmd: AccountsCmd) -> Result<()> {
    match cmd {
        AccountsCmd::List { provider } => {
            let q = provider.map(|p| format!("?provider={p}")).unwrap_or_default();
            let v = c.get(&format!("/admin/accounts{q}")).await?;
            if c.json {
                return c.print(v);
            }
            println!("{:<24} {:<12} {:<30} {:<10} {:<4} {:<12} USAGE", "ID", "PROVIDER", "LABEL", "PLAN", "PRI", "STATE");
            for a in v.as_array().into_iter().flatten() {
                let state = if !a["enabled"].as_bool().unwrap_or(true) {
                    "disabled".to_string()
                } else if a["auth_failed"].as_bool().unwrap_or(false) {
                    "re-login".to_string()
                } else if a["unsaved"].as_bool().unwrap_or(false) {
                    "UNSAVED".to_string()
                } else if a["available"].as_bool().unwrap_or(false) {
                    "ok".to_string()
                } else {
                    format!("cool→{}", fmt_ts(a.get("cooldown_until")))
                };
                let usage = a["usage"]["windows"]
                    .as_array()
                    .map(|ws| {
                        ws.iter()
                            .map(|w| format!("{} {:.0}%", w["name"].as_str().unwrap_or("?"), w["used_percent"].as_f64().unwrap_or(0.0)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "{:<24} {:<12} {:<30} {:<10} {:<4} {:<12} {}",
                    a["id"].as_str().unwrap_or(""),
                    a["provider"].as_str().unwrap_or(""),
                    a["label"].as_str().unwrap_or(""),
                    a["plan"].as_str().unwrap_or("-"),
                    a["priority"].as_i64().unwrap_or(0),
                    state,
                    usage
                );
            }
            Ok(())
        }
        AccountsCmd::Enable { id } => c.print(c.post(&format!("/admin/accounts/{id}/enable"), json!({})).await?),
        AccountsCmd::Disable { id } => c.print(c.post(&format!("/admin/accounts/{id}/disable"), json!({})).await?),
        AccountsCmd::Reset { id } => c.print(c.post(&format!("/admin/accounts/{id}/reset"), json!({})).await?),
        AccountsCmd::Remove { id } => c.print(c.delete(&format!("/admin/accounts/{id}")).await?),
        AccountsCmd::Priority { id, priority } => {
            c.print(c.post(&format!("/admin/accounts/{id}/priority"), json!({"priority": priority})).await?)
        }
        AccountsCmd::Usage { id } => c.print(c.post(&format!("/admin/accounts/{id}/usage"), json!({})).await?),
        AccountsCmd::Import { provider, file, label } => {
            let raw = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let data: Value = serde_json::from_str(&raw).context("credential file is not JSON")?;
            c.print(c.post("/admin/accounts/import", json!({"provider": provider, "data": data, "label": label})).await?)
        }
        AccountsCmd::AddKey { provider, label } => {
            let key = read_secret(&format!("{provider} API key: "))?;
            c.print(c.post("/admin/accounts/import", json!({"provider": provider, "data": {"api_key": key}, "label": label})).await?)
        }
    }
}

async fn auth(c: Client, cmd: AuthCmd) -> Result<()> {
    let AuthCmd::Login { provider, browser } = cmd;
    let start = c.post(&format!("/admin/auth/{provider}/start"), json!({"device": !browser})).await?;
    let flow_id = start["flow_id"].as_str().context("server returned no flow id")?.to_string();
    eprintln!("\n{}\n", start["instructions"].as_str().unwrap_or(""));
    eprintln!("  {}\n", start["url"].as_str().unwrap_or(""));
    if let Some(code) = start["user_code"].as_str() {
        eprintln!("  code: {code}\n");
    }
    let account = if start["needs_code"].as_bool().unwrap_or(false) {
        eprint!("Paste here: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        c.post(&format!("/admin/auth/flows/{flow_id}/complete"), json!({"code": line.trim()})).await?
    } else {
        eprintln!("Waiting for authorization...");
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let s = c.get(&format!("/admin/auth/flows/{flow_id}")).await?;
            match s["status"].as_str() {
                Some("pending") => continue,
                Some("complete") => break s,
                _ => anyhow::bail!("login failed: {}", s["error"].as_str().unwrap_or("unknown error")),
            }
        }
    };
    if c.json {
        return c.print(account);
    }
    let id = account["id"].as_str().or(account["account_id"].as_str()).unwrap_or("?");
    let email = account["email"].as_str().unwrap_or("");
    println!("Logged in {provider} account {id} {email}");
    Ok(())
}

fn read_secret(prompt: &str) -> Result<String> {
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let value = value.trim().to_string();
    anyhow::ensure!(!value.is_empty(), "no value provided");
    Ok(value)
}

/// A new password, typed twice on a terminal. Unlike other secrets it is
/// not trimmed: only stdin's trailing newline is dropped.
fn read_new_password() -> Result<String> {
    let password = if std::io::stdin().is_terminal() {
        let first = rpassword::prompt_password("New UI password: ")?;
        anyhow::ensure!(rpassword::prompt_password("Repeat it: ")? == first, "the passwords don't match");
        first
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s.strip_suffix('\n').map(|s| s.strip_suffix('\r').unwrap_or(s)).unwrap_or(&s).to_string()
    };
    anyhow::ensure!(password.chars().count() >= 12, "use at least 12 characters");
    Ok(password)
}

/// `90`, `30s`, `15m`, `24h`, `7d`.
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some('w') => (&s[..s.len() - 1], 7 * 86_400),
        _ => (s, 1),
    };
    let n: i64 = num.parse().with_context(|| format!("invalid duration {s:?}"))?;
    Ok(n * mult)
}

fn fmt_rfc3339(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

fn fmt_ts(v: Option<&Value>) -> String {
    v.and_then(Value::as_i64)
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
    json: bool,
}

impl Client {
    fn new(r: &Remote) -> Result<Self> {
        let token = r.token.clone().filter(|t| !t.is_empty()).context("set XOLOTL_ADMIN_TOKEN or pass --token")?;
        Ok(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?,
            url: r.url.trim_end_matches('/').to_string(),
            token,
            json: r.json,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.bearer_auth(&self.token).send().await.with_context(|| format!("connecting to {}", self.url))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body.pointer("/error/message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| body.to_string());
            anyhow::bail!("{status}: {msg}");
        }
        Ok(body)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send(self.http.get(format!("{}{path}", self.url))).await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(self.http.post(format!("{}{path}", self.url)).json(&body)).await
    }
    async fn put(&self, path: &str, body: Value) -> Result<Value> {
        self.send(self.http.put(format!("{}{path}", self.url)).json(&body)).await
    }
    async fn delete(&self, path: &str) -> Result<Value> {
        self.send(self.http.delete(format!("{}{path}", self.url))).await
    }

    fn print(&self, v: Value) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(&v)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
