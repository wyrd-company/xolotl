---
title: Web UI
order: 8
docs: true
---

`xolotl serve` serves an admin web UI at `/`. It does what the CLI does.

## Set up sign-in

The web UI signs in with OpenID Connect, for example with Pocket ID. Only
members of one group get in.

1. In your identity provider, add a confidential client with the callback URL
   `https://<xolotl>/admin/session/callback`. Allow the `groups` scope.
2. Make an admin group, for example `xolotl-admins`, and add yourself to it.
3. Store the client secret in OpenBao:

   ```sh
   bao kv put secret/xolotl/oidc client_secret="..."
   ```

4. Add the client to the config file:

   ```toml
   [ui.oidc]
   issuer = "https://id.example.lan"
   client_id = "..."
   redirect_url = "https://xolotl.example.lan/admin/session/callback"
   admin_group = "xolotl-admins"
   ```

5. Restart xolotl and open `https://xolotl.example.lan/`.

Xolotl checks the `groups` claim itself. A user who is not in `admin_group`
can't sign in, even if the identity provider lets them through.

A session lasts 12 hours. Sessions are in memory, so a restart signs everyone
out. Signing out of xolotl does not sign you out of the identity provider.

When the identity provider is down, use the CLI with the bootstrap admin token.

If sign-in fails, xolotl returns you to `/?signin=<reason>`: `denied`,
`expired`, `failed`, `forbidden` (not in the admin group) or `unavailable`. The
server log has the details.

## Screens

| Screen | What you can do |
|---|---|
| Overview | OpenBao and SQLite health, account pools and usage windows, totals for the last 24 hours, gateway providers, recent failed requests |
| Accounts | Accounts by provider: refresh usage, enable or disable, set priority, clear a cooldown, remove, see details |
| Add account | Codex device or browser login, Claude login, OpenCode Go key, import of a CLI credential file |
| Policies | Edit providers, scopes, model globs, precedence, winners and aliases, with a live preview of the names a key sees |
| Client keys | Issue, rotate with a grace period, enable or disable, revoke. Shows the OpenBao path, the `bao` command and the OpenBao policy the client needs. Never shows the key |
| MCP servers | Upstream servers: add, edit and remove runtime servers, set header secrets, OAuth sign-in, tools, recent tool calls |
| Providers & models | Gateway providers and their key state, set a key, the model catalog with each model's endpoints |
| Usage | The request log grouped by client, account, provider or model, and a link to `telemetry.dashboard_url` |

Every member of the admin group has full access.
