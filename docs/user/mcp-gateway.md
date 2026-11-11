---
title: MCP gateway
order: 7
product: xolotl
---

Xolotl connects to upstream MCP servers and gives clients one MCP server at
`/mcp`. A client does not see the upstream tools one by one. It sees two tools:

- `search` returns TypeScript declarations of the tools the key can call;
- `execute` runs JavaScript that calls them, in a sandbox.

The agent writes a short program that calls several tools and returns only the
result it needs. The upstream credentials stay in OpenBao.

## Add upstream servers

Upstream servers must speak Streamable HTTP. Stdio servers (`npx`, `uvx`) need
an HTTP bridge.

Each entry is one instance with its own credentials. List the same URL twice,
under two names, to use two accounts.

### In the web UI

1. Open **MCP servers** in the [web UI](web-ui.md) and select **Add server**.
2. Enter the **Name**, the **URL** and a **Description**. Pick the
   **Authentication**: **Headers** or **OAuth login**.
3. For **Headers**, select **Add header** and enter a name and a value, for
   example `Authorization` and `Bearer {token}`. `{token}` is a
   [secret](#header-secrets) that you set after you add the server.
4. Select **Add server**.

To change a server, select **Edit** on its card, then **Save**. Servers added this way, or with the
CLI, are stored in OpenBao. Changes take effect at once. You can't rename a
server: remove it and add it again.

### With the CLI

A file for `xolotl mcp put` has `name` and the fields below:

```toml
name = "linear"
url = "https://mcp.linear.app/mcp"
auth = "oauth"
```

```sh
xolotl mcp put linear.toml     # add, or change a server added this way
xolotl mcp remove linear       # refused while a policy lists it by name
```

### In the config file

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

You can't change servers from the config file at runtime.

### Fields

| Field | Meaning |
|---|---|
| `url` | The Streamable HTTP endpoint |
| `description` | Shown to clients by `search` and in the web UI |
| `headers` | Headers sent upstream. `{field}` is filled from the OpenBao secret `mcp/<name>` |
| `auth` | `headers` (default) or `oauth` |
| `oauth.client_id` | A client registered with the authorization server beforehand |
| `oauth.scopes` | Scopes to ask for. Default: what the server asks for |
| `timeout_secs` | Limit for one tool call. Default 60 |

Names are lowercase and start with a letter. In code, `-` becomes `_`, so
`github-work` is `github_work`.

## Header secrets

A header such as `Authorization = "Bearer {token}"` takes `{token}` from the
server's secret in OpenBao. On the **MCP servers** screen, each server card
lists its secret fields as **set** or **not set**. Enter a value and save it.

With the CLI:

```sh
gh auth token | xolotl mcp set-secret github-work token   # fills {token}
xolotl mcp unset-secret github-work token
```

A changed secret is used from the next call.

## OAuth logins

A server with `auth = "oauth"` has one login, shared by every key whose policy
allows that server.

On the **MCP servers** screen, select **Sign in** in the server's **OAuth**
section and approve in the browser. With the CLI, run `xolotl mcp login linear`
and open the URL it prints. What happens next depends on the configuration:

| Configuration | The browser goes to | You |
|---|---|---|
| Nothing set | `http://127.0.0.1:33418/callback`, which does not load | Paste that page's full URL into the web UI form or the CLI |
| `server.public_url` | `<public_url>/admin/mcp/oauth/callback` | Do nothing. Xolotl finishes the login |
| `mcp.oauth.redirect_uri` | That URL | Paste the URL back, unless it is xolotl's callback |

Many authorization servers accept plain-HTTP redirects only to loopback
addresses. If xolotl is not on HTTPS, use the default.

Xolotl gets a client ID in this order: `oauth.client_id`; a Client ID Metadata
Document, when `server.public_url` is HTTPS or `mcp.oauth.client_metadata_url`
is set; Dynamic Client Registration.

Xolotl refreshes the tokens itself. **Sign out** on the card, or `xolotl mcp
logout linear`, forgets them. When the server refuses the tokens, the card
shows **Sign-in needed** (`SIGN-IN` in `xolotl mcp servers`), and calls fail
until you sign in again.

## Policies

A key needs the `mcp` scope and the servers in `mcp_servers`. In the web UI's
policy editor, check **mcp (MCP gateway)** under **Scopes**, then pick the
servers and the tool globs in the **MCP servers** section. See
[Write a policy](policies-and-keys.md#write-a-policy). In a policy file:

```toml
scopes = ["proxy", "mcp"]
mcp_servers = ["github-work", "linear"]   # or ["*"]
allow_tools = ["github-work/*", "linear/list_*"]
deny_tools = ["*/delete_*"]
```

Globs match `tool` or `server/tool`. Deny wins. An empty `allow_tools` allows
every tool of the allowed servers. `xolotl policies view <policy>` lists the
tools a key can call.

Tools outside the policy are not in `search`, and a call to one fails without
reaching the upstream. Code mode has no approvals: a tool runs when the code
calls it. Use `deny_tools` to keep destructive tools away from a key.

## Code mode

### `search`

Input `{query}`. Every word must appear in a tool's server name, tool name or
description. An empty query lists every tool. The result has declarations for up
to 20 tools:

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

### `execute`

Input `{code}`, the body of an async function. The result is
`{result, logs, calls}`, or `{error, logs, calls}`.

```js
const issues = await github_work.list_issues({owner: "acme", repo: "api", state: "open"});
return JSON.parse(issues).filter((i) => i.labels.some((l) => l.name === "bug")).map((i) => i.number);
```

A tool call returns the tool's structured content, else its text, else its
content blocks. A tool error throws. `console.log` output is in `logs`.
`codemode.call(server, tool, args)` calls a tool by name.

Await every tool call you depend on. When the code returns, the gateway stops
waiting for a call it did not await. That call may never start, and then it is
not in the call log. It may also still run on the upstream, and then the call log
shows it as `cancelled`.

### Limits

Each `execute` runs in a fresh QuickJS runtime with no file system, network or
timers. The only way out is a tool call.

| Setting | Default |
|---|---|
| `mcp.execute_timeout_secs` | 120, tool calls included |
| `mcp.memory_limit_mb` | 64 |
| `mcp.max_tool_calls` | 100 |
| `mcp.max_output_bytes` | 100000; longer output is truncated |
| `mcp.max_concurrent_executions` | 8; more wait |

## Browsers

`/mcp` refuses a request with an `Origin` header unless the origin is in
`mcp.allowed_origins`. Other MCP clients send no `Origin`.

## Inspect and monitor

Each card on the **MCP servers** screen shows the server's state and tool
count; **Show tools** lists the tools. **Recent calls**, below the cards, lists
the latest tool calls of every server as `server/tool`.

With the CLI:

```sh
xolotl mcp servers               # source, state, tool count, secret fields
xolotl mcp tools github-work     # every tool, without the policy filter
xolotl mcp refresh               # re-read tool lists (also every 5 minutes)
xolotl mcp usage --group-by tool --since 7d
```

Group usage by `server`, `tool` or `client`. Every tool call is in the call log
with its status: `ok`, `tool_error`, `error` or `cancelled`.
