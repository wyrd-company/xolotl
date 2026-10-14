// Client for xolotl's /admin API. The session cookie is HttpOnly; the CSRF
// token from sign-in goes in `x-csrf-token` on every request.

let csrfToken: string | null = null;
let onUnauthorized: (() => void) | null = null;

export function setCsrfToken(token: string | null) {
  csrfToken = token;
}

/** Called when a request comes back 401, e.g. the session expired. */
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

export async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { accept: "application/json" };
  if (csrfToken) headers["x-csrf-token"] = csrfToken;
  if (body !== undefined) headers["content-type"] = "application/json";
  const resp = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  const text = await resp.text();
  let data: unknown = undefined;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = text;
  }
  if (!resp.ok) {
    const msg =
      (data as { error?: { message?: string } } | undefined)?.error?.message ??
      (typeof data === "string" && data ? data : resp.statusText);
    if (resp.status === 401 && path !== "/admin/session") onUnauthorized?.();
    throw new ApiError(resp.status, msg);
  }
  return data as T;
}

export const get = <T>(path: string) => api<T>("GET", path);
export const post = <T>(path: string, body: unknown = {}) => api<T>("POST", path, body);
export const put = <T>(path: string, body: unknown) => api<T>("PUT", path, body);
export const del = <T>(path: string) => api<T>("DELETE", path);

// ---- types (mirroring the Rust structs)

export type SubProvider = "claude" | "codex" | "grok" | "opencode-go";
export const SUB_PROVIDERS: SubProvider[] = ["claude", "codex", "grok", "opencode-go"];
export const PROVIDER_NAMES: Record<SubProvider, string> = { claude: "Claude", codex: "Codex", grok: "Grok", "opencode-go": "OpenCode Go" };

export interface Session {
  authenticated: boolean;
  csrf_token?: string;
  password_set?: boolean;
}

export interface Health {
  ok: boolean;
  error?: string;
  note?: string;
}

export interface LoggedRequest {
  ts: number;
  client_id: string | null;
  provider: string;
  account_id: string | null;
  model: string | null;
  endpoint: string;
  status: number;
  latency_ms: number;
  trace_id: string | null;
}

export interface Status {
  version: string;
  database: Health;
  vault: Health;
  catalog_refreshed_secs_ago: number | null;
  unsaved_credentials: number;
  recent_errors: LoggedRequest[];
  dashboard_url: string | null;
  vault_mount: string;
  vault_prefix: string;
  subscription: { strategy: string; soft_limit_percent: number; session_affinity_secs: number; usage_poll_secs: number };
}

export interface UsageWindow {
  name: string;
  used_percent: number;
  resets_at: number | null;
  window_secs: number | null;
}

export interface Account {
  id: string;
  provider: SubProvider;
  label: string;
  email: string | null;
  external_id: string | null;
  plan: string | null;
  enabled: boolean;
  priority: number;
  cooldown_until: number | null;
  cooldown_reason: string | null;
  auth_failed: boolean;
  last_error: string | null;
  last_used_at: number | null;
  created_at: number;
  auth_index: string;
  available: boolean;
  expires_at: number | null;
  unsaved: boolean;
  usage: { account_id: string; fetched_at: number; windows: UsageWindow[] } | null;
}

export interface FlowStart {
  flow_id: string;
  provider: SubProvider;
  url: string;
  user_code?: string;
  needs_code: boolean;
  instructions: string;
}

export type FlowStatus =
  | { status: "pending" }
  | { status: "complete"; account_id: string; email: string | null }
  | { status: "failed"; error: string };

export type Scope = "proxy" | "management" | "mcp" | "admin";

export interface Policy {
  name: string;
  description?: string;
  providers: string[];
  allow_models?: string[];
  deny_models?: string[];
  precedence?: string[];
  winners?: Record<string, string>;
  aliases?: Record<string, string | string[]>;
  scopes: Scope[];
  mcp_servers?: string[];
  allow_tools?: string[];
  deny_tools?: string[];
}

export interface PolicyView {
  policy: string;
  models: { name: string; targets: string[]; endpoints: string[]; translated_endpoints: string[]; alias: boolean }[];
  shared: { model: string; providers: string[]; winner: string }[];
  mcp_servers: string[];
  /** `server/tool` names the policy allows. */
  tools: string[];
}

export interface KeyInfo {
  client_id: string;
  policy: string;
  name: string;
  description: string;
  vault_path: string;
  created_at: string | null;
  rotated_at: string | null;
  expires_at: string | null;
  disabled: boolean;
  previous_expires_at?: string;
}

export interface GatewayProvider {
  name: string;
  kind: "vercel" | "mistral" | "openai-compatible";
  enabled: boolean;
  base_url: string | null;
  no_auth: boolean;
  secret: string;
  has_key: boolean;
}

export interface CatalogRoute {
  provider: string;
  model: string;
  endpoints: string[];
  translated_endpoints: string[];
}

export type GroupBy = "client" | "account" | "provider" | "model";

export interface UsageRow {
  key: string | null;
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
}

export interface McpServer {
  name: string;
  url: string;
  description: string;
  /** The global name in code mode. */
  namespace: string;
  secret: string;
  /** Header placeholder fields and whether each is set. */
  secret_fields: Record<string, boolean>;
  connected: boolean;
  server_info: string | null;
  protocol_version: string | null;
  tools: number;
  tools_refreshed_at: number | null;
  last_error: string | null;
  auth: "headers" | "oauth";
  oauth?: McpOAuthStatus;
  /** `config`: from the config file, read-only here. `vault`: added at runtime. */
  source: "config" | "vault";
  definition?: McpServerDefinition;
}

/** The fields of a `[mcp.servers.<name>]` entry. */
export interface McpServerDefinition {
  url: string;
  description?: string;
  headers?: Record<string, string>;
  timeout_secs?: number | null;
  auth?: "headers" | "oauth";
  oauth?: { client_id?: string | null; scopes?: string[] };
}

export interface McpOAuthStatus {
  signed_in: boolean;
  signed_in_at: number | null;
  expires_at: number | null;
  refreshable: boolean;
  scopes: string[];
  registration: "pre-registered" | "client-metadata-document" | "dynamic" | null;
  issuer: string | null;
  needs_login: string | null;
  unsaved: boolean;
}

export interface McpLoginStart {
  flow_id: string;
  server: string;
  url: string;
  redirect_uri: string;
  callback: boolean;
  registration: NonNullable<McpOAuthStatus["registration"]>;
}

export type McpLoginStatus = { server: string } & ({ status: "pending" } | { status: "complete" } | { status: "failed"; error: string });

export interface McpTool {
  name: string;
  title?: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  annotations?: { readOnlyHint?: boolean; destructiveHint?: boolean; title?: string };
}

export type McpGroupBy = "server" | "tool" | "client";

export interface McpUsageRow {
  key: string | null;
  calls: number;
  errors: number;
  avg_latency_ms: number;
}

export interface McpCall {
  ts: number;
  client_id: string | null;
  server: string;
  tool: string;
  status: "ok" | "tool_error" | "error";
  error: string | null;
  latency_ms: number;
  trace_id: string | null;
}
