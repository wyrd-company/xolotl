---
xolotl: minor
---

Add an MCP gateway: upstream Streamable HTTP MCP servers from `[mcp.servers]` (several instances of one server with different credentials, header secrets in OpenBao) exposed to clients at `/mcp` as a code-mode MCP server with `search` and `execute` tools. Policies gain the `mcp` scope, `mcp_servers`, `allow_tools` and `deny_tools`. Includes an MCP call log, metrics, `xolotl mcp` commands and an MCP servers page in the web UI.

Upstream servers can use OAuth logins (`auth = "oauth"`, `xolotl mcp login`): discovery, pre-registered clients, client metadata documents or dynamic registration, PKCE, token refresh, with a paste-back or `server.public_url` callback finish.

MCP servers can also be added, changed and removed at runtime from the web UI or `xolotl mcp put` / `remove`; they are stored in OpenBao, and removal is refused while a policy lists the server.
