# MCP gateway

Xolotl connects to upstream MCP servers and exposes them to clients through one MCP server at `/mcp`. A client doesn't see the upstream tools one by one. It sees two code-mode tools: `search` returns TypeScript declarations of the tools it may call, and `execute` runs JavaScript that calls them. `src/mcp/` holds the implementation.

```
MCP client ── /mcp (client key, mcp scope) ── search / execute
                                                   │  QuickJS sandbox
                                                   ▼
                       policy: mcp_servers, allow_tools, deny_tools
                                                   │
                     upstream servers (Streamable HTTP, headers from OpenBao)
```

## Protocol

[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) (the official Rust SDK) provides both sides.

| Side | Behaviour |
|---|---|
| `/mcp` (server) | Serves MCP 2026-07-28 statelessly. Older clients can still use the `initialize` handshake, and they get no session either; sessions were optional before 2026-07-28. Any instance can serve any request. Replies are JSON. |
| Upstream (client) | Probes each server with `server/discover` (2026-07-28). A legacy server gets `initialize` at 2025-11-25, and an expired session is re-initialized. A server that needs user input (`input_required`, elicitation, sampling) or returns a task fails the call, because there is no user to ask. |

Only Streamable HTTP upstreams are supported. Stdio servers (`npx`, `uvx`) are not: the container image has no Node or Python. To use one, run it behind an HTTP bridge.

## Upstream servers

Servers come from two places, keyed by name:

- **Config file:** `[mcp.servers.<name>]` entries. They are read-only at runtime.
- **OpenBao:** servers added with the web UI's **Add server** form or `xolotl mcp put`, stored at `mcp-servers/<name>`.

A config-file entry looks like this:

```toml
[mcp.servers.github-work]
url = "https://api.githubcopilot.com/mcp/"
description = "GitHub, work account"
headers = { Authorization = "Bearer {token}" }

[mcp.servers.github-personal]
url = "https://api.githubcopilot.com/mcp/"
description = "GitHub, personal account"
headers = { Authorization = "Bearer {token}" }
```

- **Servers in OpenBao:** a definition has the same fields as a config entry, plus `name`. For example, a TOML file for `xolotl mcp put`:

  ```toml
  name = "linear"
  url = "https://mcp.linear.app/mcp"
  auth = "oauth"
  ```

  - **Applying changes:** changes take effect at once. Other instances pick them up within 60 s, the same sync that reloads keys and policies.
  - **Reconnecting:** a changed server reconnects and lists its tools again. An unchanged one keeps its connection and login.
  - **Renaming:** not possible; remove the server and add it again.
- **Rules for servers in OpenBao:**
  - A name the config file already uses is refused, and so is one that collides in code with an existing server.
  - A server written to OpenBao by hand with a config-file name, or with an invalid definition, is skipped with a warning.
  - Removing a server (`xolotl mcp remove`, or the UI) also deletes its `mcp/<name>` secret and its OAuth login.
  - Removal is refused while a policy lists the server by name; a policy with `mcp_servers = ["*"]` doesn't block it.
- **Instances:** each entry is one instance. Listing the same URL twice under two names gives two sets of credentials, and a policy can allow one, the other, or both.
- **Authentication:** `auth = "headers"` (the default) sends the configured headers; `auth = "oauth"` signs in with OAuth (see [OAuth logins](#oauth-logins)) and sends the headers too.
- **Secrets:** `{field}` in a header value is filled from the vault secret `mcp/<name>`.
  - Set fields with `xolotl mcp set-secret <server> <field>` (the value is read from stdin) or on the web UI's MCP page.
  - Remove them with `xolotl mcp unset-secret`.
  - Changing a secret drops the connection, so the next call reconnects with the new value.
  - Fields are merged with check-and-set and retried on a conflict, so concurrent updates of different fields all survive.
- **Names:** lowercase, starting with a letter. In code a name becomes a JavaScript identifier with `-` replaced by `_`, so `github-work` is `github_work`. Names that collide after that replacement are rejected. `mcp` is also reserved as a gateway provider name.
- **Connections:** opened on first use. A transport failure drops the connection and the next call reconnects. Redirects are never followed, so the headers only reach the configured origin. Each tool call is limited by `timeout_secs` (default 60).
- **Tool lists:** read at startup, every `mcp.tools_refresh_secs` (300), on `xolotl mcp refresh`, and the first time a policy needs a server that hasn't been listed yet. If a refresh fails, the previous list is kept. `list_changed` notifications are not subscribed to.

## OAuth logins

A server with `auth = "oauth"` gets one login per instance, shared by every key whose policy allows that instance. Two entries for the same URL are two logins, for example a work account and a personal one. `src/mcp/oauth.rs` follows the MCP authorization spec through rmcp's `auth` feature.

```toml
[mcp.servers.linear]
url = "https://mcp.linear.app/mcp"
auth = "oauth"
# oauth.client_id = "…"     # a client registered with the authorization server beforehand
# oauth.scopes = ["read"]   # default: what the server asks for
```

### Signing in

Sign in with `xolotl mcp login <server>`, or with **Sign in** on the web UI's MCP page.

1. Xolotl sends the server an unauthenticated request. The 401's `WWW-Authenticate` header points to the server's protected resource metadata (RFC 9728), which names the authorization server. Xolotl falls back to well-known URLs when the 401 doesn't say.
2. Xolotl gets a client id, in the order the spec gives:
   - **pre-registered:** `oauth.client_id`, with an optional `client_secret` field in the vault secret `mcp/<name>`;
   - **Client ID Metadata Document:** when the authorization server supports it and xolotl has a document URL (see below);
   - **Dynamic Client Registration:** the registration is stored and reused for the same issuer and redirect URI.
3. You open the authorization URL and approve. The request uses PKCE (S256), and both the authorization and token requests carry `resource` set to the server URL (RFC 8707).
4. The browser comes back with a code. Xolotl checks `state` and `iss` (RFC 9207) before it exchanges the code.

Where the browser goes in step 4 depends on the config:

| Config | Redirect URI | Finishing |
|---|---|---|
| neither option set | `http://127.0.0.1:33418/callback` | Nothing listens there, so the page fails to load. Paste its URL into the CLI or the UI. |
| `server.public_url` | `<public_url>/admin/mcp/oauth/callback` | Xolotl finishes the login itself. The CLI and UI wait for it. |
| `mcp.oauth.redirect_uri` | that URL | Paste the URL back, unless it points at xolotl's callback. |

Notes on the redirect:

- **Plain HTTP:** many authorization servers accept plain-HTTP redirects only to loopback addresses. Use the paste-back default when xolotl isn't on HTTPS.
- **Callback security:** the callback is unauthenticated. What authorizes it is the unguessable `state` of a login an admin started. A login completes once and expires after 15 minutes.

**Client ID Metadata Document:** xolotl serves its document at `/admin/mcp/oauth/client-metadata.json`, unauthenticated.

- When `server.public_url` is HTTPS, that URL is the client id.
- Otherwise, host the same document on any HTTPS URL and set `mcp.oauth.client_metadata_url` to it. The document's `client_id` must equal that URL; xolotl's copy uses `mcp.oauth.client_metadata_url` as the `client_id` when it is set.

### Tokens

Tokens live in the vault at `mcp/<name>/oauth` together with the client registration.

- **Use:** every request to the server carries the access token.
- **Refresh:** the token is refreshed shortly before it expires, and once more when the server answers 401.
- **Serialized:** refreshes are serialized per server, because refresh tokens may be single-use.
- **Re-read before refreshing:** before each refresh, xolotl reads the vault again, so a refresh another instance made is used rather than repeated with a spent refresh token.
- **Generations:** the record carries a login generation, which moves on with each login, sign-out and removal.
  - Credentials obtained under an earlier generation are never saved. That covers a login started before a sign-out, a refresh racing a sign-out, and a login started for a server that was since removed and re-created.
  - Logins in progress are cancelled when the server is signed out, removed or replaced.
- **Other instances:** writes use check-and-set. On a conflict, the stored record wins when its generation differs, for example after a sign-out or new login elsewhere. Within the same generation, the newer token wins. The 60 s sync re-reads the record, so a sign-out elsewhere also reaches a connection that is already open.
- **Vault failures:** a token whose vault write fails keeps working from memory. It shows as unsaved and is retried every 15 s, through the same check-and-set, so a sign-out or removal that happened meanwhile is kept.
- **Editing a server:** changing only fields the login doesn't depend on (description, headers, timeout) keeps the login. Changing the URL, the auth mode or the OAuth settings starts a new one.
  - Through the API, the old login is deleted.
  - The login record also carries a fingerprint of the URL, client id and scopes it was made for. A login whose fingerprint doesn't match the server's current settings is never used, which covers config-file edits across a restart.

These mark the server **sign-in needed**:

- a rejected refresh token;
- a 401 after refreshing;
- a 403 `insufficient_scope`;
- a change of authorization server.

Calls to the server then fail at once until someone signs in again. Signing in asks for the scopes the server wants. `xolotl mcp logout` forgets the tokens and keeps the registration.

A server set to `auth = "headers"` that answers with an OAuth challenge gets an error suggesting `auth = "oauth"`.

## Policies

Three policy fields, plus the `mcp` scope:

```toml
scopes = ["proxy", "mcp"]
mcp_servers = ["github-work", "linear"]   # or ["*"]
allow_tools = ["github-work/*", "linear/list_*"]
deny_tools = ["*/delete_*"]
```

- **Matching:** globs match `tool` or `server/tool`. Deny wins over allow. An empty `allow_tools` allows every tool of the allowed servers.
- **Scope:** `mcp_servers` requires the `mcp` scope.
- **MCP-only policies:** a policy may list no LLM `providers` when it allows MCP servers.
- **Inspecting:** `xolotl policies view` and the web UI's policy editor list the tools a key under the policy can call.

Tools outside the policy are left out of `search`. A call to one throws inside the code without reaching the upstream. A server outside the policy doesn't exist as a global.

## Code mode

`/mcp` lists two tools.

### `search`

**Input:** `{query}`. Every word of the query must appear in a tool's server name, tool name or description; an empty query lists every tool.

**Output:** declarations for up to 20 matching tools, followed by the names of any further matches:

```ts
/** github-work: GitHub, work account */
declare const github_work: {
  /** Create an issue */
  create_issue(args: {
    owner: string;
    repo: string;
    title: string;
    body?: string;
  }): Promise<unknown>;
};
```

`codemode.call(server, tool, args)` calls any tool by name. The server can be given by its name or its identifier. Tools named `then` or `toJSON` need it: a server object never exposes those names, so it isn't mistaken for a promise or turned into JSON. Their declarations say so, and the policy and limits apply as for any call.

Input schemas become TypeScript types, and so do output schemas. Property descriptions become doc comments. Unsupported schema constructs become `unknown`. Tool names that aren't identifiers are quoted (`"get-item"(args)`) and are called as `ns["get-item"](...)`; the identifier form `ns.get_item(...)` works too.

### `execute`

**Input:** `{code}`, the body of an async function. An `async () => {...}` expression also works.

**Output:** `{result, logs, calls}`, or `{error, logs, calls}` with `isError` set. `calls` lists each tool call with its duration and outcome.

```js
const issues = await github_work.list_issues({owner: "acme", repo: "api", state: "open"});
return JSON.parse(issues).filter((i) => i.labels.some((l) => l.name === "bug")).map((i) => i.number);
```

What a tool call resolves to:

- the tool's `structuredContent`, if it returned any;
- otherwise its text, with several text blocks joined by newlines;
- otherwise the content blocks themselves (images and other content).

A result with `isError` is thrown as an `Error`, and so is a failed call. `console.log`, `info`, `warn` and `error` output is captured.

The tool's description in `tools/list` names the servers available to the caller.

### Sandbox

Each execution gets a fresh [QuickJS](https://bellard.org/quickjs/) runtime (through `rquickjs`) on a blocking thread. Tool calls run on the main runtime.

- **No I/O built-ins:** the runtime has no file system, network, timers or modules. The only way out is a tool call, and the policy checks each call again.
- **Deadline:** `mcp.execute_timeout_secs` (120), wall clock, tool calls included. Busy JavaScript is interrupted, pending tool calls fail, and code that awaits something that never settles is stopped.
- **Memory:** `mcp.memory_limit_mb` (64) of heap.
- **Tool calls:** `mcp.max_tool_calls` (100) per execution.
- **Output:** `mcp.max_output_bytes` (100 000) for the reply; logs get half of that. Larger output is truncated.
- **Concurrency:** `mcp.max_concurrent_executions` (8). Further executions wait.

Code mode has no approvals and no durable replay: a tool call happens when the code makes it. Use `deny_tools` to keep destructive tools away from a key.

## Endpoint security

- **Authentication:** `/mcp` takes a client key in `Authorization: Bearer`. A 401 carries `WWW-Authenticate: Bearer`. The OAuth resource-server profile of the MCP authorization spec is not implemented: keys are issued by xolotl.
- **Origin:** a request with an `Origin` header is refused unless the origin is in `mcp.allowed_origins` (empty by default), so web pages can't call the endpoint from a browser.
- **Host:** the `Host` header is not checked. A key, not an ambient credential, authorizes a request, so DNS rebinding gains nothing.

## Call log and metrics

Every upstream tool call is logged to SQLite table `mcp_call_log` with client, server, tool, status, error, latency and trace id. Status is one of:

- `ok`;
- `tool_error`: the tool ran and reported an error;
- `error`: there is no result (connection failure, timeout, protocol error);
- `cancelled`: the execution ended (deadline, return or client gone) before a result arrived. The upstream may still have carried the call out, so its outcome is unknown.

Calls refused by the policy never reach an upstream and aren't logged. The log is pruned with the request log (`database.retention_days`).

| Where | What |
|---|---|
| `xolotl mcp usage --group-by server\|tool\|client` | calls, errors and average latency |
| `GET /admin/mcp/usage`, `GET /admin/mcp/calls` | the same, plus recent calls, for the web UI |
| `xolotl.mcp.tool_calls` | counter by server, tool and status |
| `xolotl.mcp.tool_call.duration` | histogram by server and tool |
| `xolotl.mcp.executions` | counter by outcome |

## Admin API

| Endpoint | Does |
|---|---|
| `GET /admin/mcp/servers` | servers with their source (`config` or `vault`, with the stored definition), connection state, server info, protocol version, tool count, last error, and which secret fields are set |
| `GET /admin/mcp/servers/<name>/tools` | the server's full tool list |
| `POST /admin/mcp/servers/<name>/refresh`, `POST /admin/mcp/refresh` | re-read tool lists |
| `POST /admin/mcp/servers/<name>/oauth/start` | start a login: the authorization URL, the redirect URI, whether xolotl's callback finishes it, the registration used |
| `GET /admin/mcp/oauth/flows/<id>`, `POST /admin/mcp/oauth/flows/<id>/complete` | a login's state; `{"redirect_url": "…"}` finishes a pasted-back login |
| `DELETE /admin/mcp/servers/<name>/oauth` | sign out |
| `GET /admin/mcp/oauth/callback`, `GET /admin/mcp/oauth/client-metadata.json` | unauthenticated: the redirect target and the client metadata document |
| `POST /admin/mcp/servers`, `PUT /admin/mcp/servers/<name>`, `DELETE /admin/mcp/servers/<name>` | add, change or remove a server stored in OpenBao (`{"name": …, "url": …, …}`); 409 for config-file servers, taken names, and removals while a policy lists the server |
| `PUT /admin/mcp/servers/<name>/secret` | `{"fields": {"token": "…", "old": null}}` merges into `mcp/<name>`; `null` removes a field |
| `GET /admin/mcp/usage`, `GET /admin/mcp/calls` | call log |
