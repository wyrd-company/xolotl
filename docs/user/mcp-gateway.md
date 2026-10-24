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

### At runtime

Servers added with `xolotl mcp put` or the web UI's **Add server** form are
stored in OpenBao. A file for `xolotl mcp put` has `name` and the same fields:

```toml
name = "linear"
url = "https://mcp.linear.app/mcp"
auth = "oauth"
```

```sh
xolotl mcp put linear.toml     # add, or change a server added this way
xolotl mcp remove linear       # refused while a policy lists it by name
```

Changes take effect at once. You can't change servers from the config file at
runtime, and you can't rename a server: remove it and add it again.

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

```sh
gh auth token | xolotl mcp set-secret github-work token   # fills {token}
xolotl mcp unset-secret github-work token
```

You can also set them on the web UI's MCP page. A changed secret is used from
the next call.

## OAuth logins

A server with `auth = "oauth"` has one login, shared by every key whose policy
allows that server.

```sh
xolotl mcp login linear
```

Open the URL the CLI prints and approve. What happens next depends on the
configuration:

| Configuration | The browser goes to | You |
|---|---|---|
| Nothing set | `http://127.0.0.1:33418/callback`, which does not load | Paste that page's URL into the CLI |
| `server.public_url` | `<public_url>/admin/mcp/oauth/callback` | Do nothing. Xolotl finishes the login |
| `mcp.oauth.redirect_uri` | That URL | Paste the URL back, unless it is xolotl's callback |

Many authorization servers accept plain-HTTP redirects only to loopback
addresses. If xolotl is not on HTTPS, use the default.

Xolotl gets a client ID in this order: `oauth.client_id`; a Client ID Metadata
Document, when `server.public_url` is HTTPS or `mcp.oauth.client_metadata_url`
is set; Dynamic Client Registration.

Xolotl refreshes the tokens itself. `xolotl mcp logout linear` forgets them.
When the server refuses the tokens, `xolotl mcp servers` shows `SIGN-IN` and
calls fail until you log in again.

## Policies

A key needs the `mcp` scope and the servers in `mcp_servers`:

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

```sh
xolotl mcp servers               # source, state, tool count, secret fields
xolotl mcp tools github-work     # every tool, without the policy filter
xolotl mcp refresh               # re-read tool lists (also every 5 minutes)
xolotl mcp usage --group-by tool --since 7d
```

Group usage by `server`, `tool` or `client`. Every tool call is in the call log
with its status: `ok`, `tool_error`, `error` or `cancelled`. The web UI shows
recent calls.
