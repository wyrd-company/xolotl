---
relationships:
  implemented-by: layout
---
# Web UI

`xolotl serve` serves an admin web UI at `/`. It is a client of the same `/admin` API the CLI uses, plus a few endpoints added for it.

The shared sidebar includes a visible **Repository / report issues** anchor to
`https://github.com/wyrd-company/xolotl`. It opens a new tab with `noopener
noreferrer`, names that behavior for screen readers, and has a visible keyboard
focus outline. The link remains available on narrow screens and every signed-in
route.

## CLI parity

This inventory compares the command declarations and handlers in
[the CLI](../../src/cli.rs), the [admin API](../../src/server/admin.rs), and all
[UI pages](../../ui/src/pages). It describes the implementation at the 0.7.0 epic
base, rather than asserting browser acceptance. The standard container uses a
distroless runtime with no shell; routine management requires a browser workflow.
The [parity issue](https://github.com/wyrd-company/xolotl/issues/138) defines that
workflow's acceptance requirement.

Each row lists its local options and positional arguments. Angle brackets name
arguments; optionality, defaults and accepted values are explained below. Every
remote command also accepts the shared connection/output options. **Equal** means
the source exposes the same meaningful status, results and error details in a
browser form; it does not promise identical JSON or formatting. **Partial** names
the missing capability or feedback. **Gap** means there is no UI equivalent.
**Excluded** gives the reason a command has no browser workflow. A group row is a
navigation entry, with its actions assessed in the child rows.

### Shared options and feedback

| CLI option | Browser equivalent and assessment |
|---|---|
| `--url` / `XOLOTL_URL` (default `http://127.0.0.1:8317`) | Excluded as a form control: open the deployment's URL; UI requests use the same origin. |
| `--token` / `XOLOTL_ADMIN_TOKEN` | OIDC sign-in creates an admin session with CSRF protection. Excluded as a token-entry control: bootstrap recovery and keys-scoped CLI principals are separate access methods; the UI grants full administration to its configured OIDC group. |
| `--json` | Excluded as a literal output format: the browser presents data as controls and tables. Meaningful fields in JSON still require a visible equivalent; the rows below include gaps discovered by reading the response bodies. |
| `-h`, `--help` | Excluded as parser controls: the UI labels its controls and links to documentation; help subcommands dispatch to the same command help, recursively. |
| `-V`, `--version` | Excluded as a local binary inspection: the running deployment's version is shown in the sidebar from `/admin/status`. |

The CLI reports non-success HTTP status and the API's `error.message`. The shared
UI [API client](../../ui/src/lib/api.ts) preserves that message; mutation actions
toast it through [useAction](../../ui/src/lib/hooks.ts), and queries display it
only where a `QueryError` is rendered. Numeric HTTP status remains in `ApiError`
rather than its rendered message. Treat absence of a visible query error as a
feedback gap, even when the API request fails correctly. A successful HTTP result
can contain an embedded warning, discovery failure or per-server error; a generic
success toast does not account for those fields.

### Server and command navigation

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl` | — | Sidebar navigation | Excluded: local parser entry; the command groups below map its navigation. |
| `xolotl serve` | `--config` | Deployment configuration | Excluded: launches the process hosting the UI. `-c` / `--config` and `XOLOTL_CONFIG` select the local config file; starting or changing that process is a deployment operation. |
| `xolotl check-config` | `--config` | Deployment config validation | Excluded: validates a local file and prints redacted config without a running server. `-c` / `--config` and `XOLOTL_CONFIG` select that file. |
| `xolotl keys` | — | Client keys | Excluded: command-group navigation; child actions below. |
| `xolotl policies` | — | Policies | Excluded: command-group navigation; child actions below. |
| `xolotl accounts` | — | Accounts / Add account | Excluded: command-group navigation; child actions below. |
| `xolotl auth` | — | Add account / account details | Excluded: command-group navigation; child action below. |
| `xolotl providers` | — | Accounts, Providers tab | Excluded: command-group navigation; child action below. |
| `xolotl mcp` | — | MCP servers | Excluded: command-group navigation; child actions below. |
| `xolotl reload` | — | Overview: Deployment → Reload from OpenBao | Equal: explicit vault reload shows loaded policy, client key, pool account and provider-account counts, including zero, or the underlying failure with retry guidance. Pending reload disables resubmission. Model discovery has its own status and refresh control; reload completion does not claim discovery completion. |
| `xolotl help` | `<command>` | Documentation and control labels | Excluded: clap-generated command help; a command path can be supplied recursively, including `help help`. |

### Client keys

Source: [keys page](../../ui/src/pages/keys.tsx). Key IDs select policy/name rows;
neither interface prints the secret. CLI duration arguments accept numbers with
`s`, `m`, `h`, `d` or `w` suffixes.

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl keys issue` | `--description --name --policy --ttl` | Issue a key: policy, client name, description, expiry | Equal: policy/name and optional description, Never by default, presets and custom positive duration (s/m/h/d/w or seconds). Invalid custom expiry stays in the form with an inline error. Issued/exists, expiry and correct vault retrieval instructions are visible; underlying mutation errors are surfaced. Policy-list failures show their cause and Retry and disable issuance. Deployment-status failures show Retry and withhold retrieval commands until the configured mount/prefix are available. |
| `xolotl keys list` | — | Client keys table | Equal: identity, description (also during rotation grace), active/disabled/expired/grace state, vault path, creation/rotation/expiry and grace end are visible. Never-rotated keys explicitly use creation as the rotation reference. Every current request/token minute/day counter is shown, with policy totals only for limited windows and a shared-policy label. The additional 7d requests query reports failures as unavailable, with cause and Retry, rather than zero. Key-list and deployment-status failures also show cause and Retry. |
| `xolotl keys rotate` | `--grace <id>` | Rotate dialog on the selected key | Equal: grace defaults to zero, with presets and arbitrary nonnegative duration (s/m/h/d/w or seconds). Invalid custom grace remains editable with an inline error. Result refreshes the row with grace end, rotation time and expiry; underlying mutation errors are surfaced. |
| `xolotl keys revoke` | `<id>` | Revoke confirmation on a row or by client ID | Equal: confirmation describes destruction; server results distinguish revoked, already revoked and absent, with underlying errors. Revoke by client ID allows repeat requests after the row disappears; the latest outcome remains visible. |
| `xolotl keys enable` | `<id>` | Enable on a disabled key | Equal: success toast, refreshed key state, underlying API error. |
| `xolotl keys disable` | `<id>` | Disable on an enabled key | Equal: success toast, refreshed key state, underlying API error. |

### Policies

Sources: [editor](../../ui/src/pages/policies.tsx), [model list](../../ui/src/pages/policy-model-list.tsx)
and [provider/account selection](../../ui/src/pages/policy-providers.tsx).

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl policies list` | — | Policy navigation and selected editor; unloaded-policy warning | Partial: providers, scopes and description can be inspected in the editor; unloaded policies show their name, underlying error and effect on keys. The status query has no visible error if the unloaded-policy check itself fails; CLI explicitly warns about that failure. Group: policy inspection. |
| `xolotl policies get` | `<name>` | Select policy in editor | Equal: the editor exposes the current policy fields: identity, description, providers/accounts, ordered rules, model switches/default, precedence, winners, aliases/fallbacks, renames, scopes, MCP access, key-policy restrictions and limits. List query failure is displayed. JSON file export is a formatting exclusion. |
| `xolotl policies put` | `<file>` | New policy / Save policy | Partial: structured editor represents policy fields; local TOML/JSON file parsing is replaced by browser controls. Preview validation and mutation errors are visible, and empty allowed pools have an inline warning. The save response's `warnings` array is discarded, so server warning text is not assured to reach the operator. Group: policy inspection. |
| `xolotl policies delete` | `<name>` | Delete policy confirmation | Partial: for a loaded policy the key-use restriction is visible, deletion removes the editor and mutation errors are surfaced. CLI can also delete a stored policy that failed to load, subject to the same key-use restriction; the UI offers no deletion control for an unloaded policy. Group: policy inspection. |
| `xolotl policies view` | `<name>` | Effective models, aliases, provider accounts and MCP tools in policy preview | Partial: shows decisions, names, provider winner/health, alias targets/fallbacks and allowed tool names. Does not render the resolved `models` routing rows with native and translated endpoint lists that CLI prints, or the explicit accounts-read failure when `accounts` is null. Preview errors are visible. Group: policy inspection. |

### Accounts, authentication and provider credentials

Sources: [accounts](../../ui/src/pages/accounts.tsx), [add account/login/import](../../ui/src/pages/add-account.tsx)
and [provider accounts](../../ui/src/pages/provider-accounts.tsx). Selected rows
provide the account ID or provider name rather than requiring it to be typed.

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl accounts list` | `--provider` | All, subscription-provider and Providers tabs | Partial: pool ID, label/email, plan, priority, usage windows and provider kind/source/key state are shown. CLI prints UNSAVED for an account with unsaved refreshed tokens; the account row has no corresponding badge and can read Available. The unsaved-token warning is visible in account details and as an Overview count. Optional provider filter accepts the subscription provider enum. Account/provider query errors are shown. Group: account recovery and usage. |
| `xolotl accounts enable` | `<id>` | Enable on account row | Equal: state refresh, success and underlying API errors; discovery feedback is visible in shared Refresh models status. |
| `xolotl accounts disable` | `<id>` | Disable on account row | Equal: state refresh, success and underlying API errors; discovery feedback is visible in shared Refresh models status. |
| `xolotl accounts reset` | `<id>` | Retry in details | Partial: Retry is enabled only while cooling down or auth-failed; CLI can reset any existing account. When available, it clears cooldown/auth failure, reports discovery outcome separately and preserves enabled state; errors and refreshed state are visible. Group: account recovery and usage. |
| `xolotl accounts remove` | `<id>` | Remove in details | Equal: confirmation, removed row, underlying API error. |
| `xolotl accounts priority` | `<id> <priority>` | Priority in details | Partial: numeric control and success/error feedback exist. CLI accepts signed 64-bit priority; JavaScript Number cannot preserve every integer in that range. Group: account form inputs. |
| `xolotl accounts usage` | `<id>` | Refresh usage | Partial: Refresh usage is disabled for a disabled account; CLI can poll it. Public windows, age and Claude/Codex reset credits are visible, with success and upstream errors. CLI prints the returned usage JSON including provider-specific `raw` fields; details have no way to inspect other raw diagnostics. The internal `iguana_necktie` bucket is deliberately filtered by both interfaces' API source; that exclusion does not require a new window control. Group: account recovery and usage. |
| `xolotl accounts import` | `--label <provider> <file>` | Import a login: provider and local JSON file | Partial: file upload, JSON error and API errors are visible, imported identity appears in Accounts. Optional `--label` has no UI field. Import discards the response's `catalog_refresh`, so an immediate discovery failure is hidden; shared catalog polling can expose it later. CLI enum includes providers whose import support the server rejects; the UI offers account kinds marked importable. Groups: account form inputs; account login feedback. |
| `xolotl accounts add-key` | `--label --provider` | Add account, API-key pool form | Equal: provider (CLI defaults to opencode-go), optional label and password input replace stdin. A generic "Account added" confirmation and discovery result/error are visible; inspect the new account ID and label on Accounts. Underlying mutation errors are visible. Unsupported enum/provider combinations are rejected by the API in CLI; UI shows supported kinds. |
| `xolotl accounts add-provider` | `--base-url --header --kind --no-auth <name>` | Add account, provider form | Equal: name, required kind, optional base URL, no-auth and extra headers (UI accepts `Name: value` lines; CLI accepts repeated `--header Name=value`); kind choices are anthropic/openai/vercel/mistral/openai-compatible. Password input replaces stdin unless no-auth. Validation, creation and discovery errors/counts are visible. |
| `xolotl accounts remove-provider` | `<name>` | Remove on a runtime provider row | Equal: config-owned rows cannot be removed; confirmation, policy-use error or successful removal is visible. |
| `xolotl auth login` | `--browser <provider>` | Add account and Log in again in details | Partial: Codex device/default and browser/paste flows, Claude paste and Grok device flows map; pending/completed/failed status from successful poll responses is visible. HTTP polling errors are not rendered in DeviceCode; CLI stops with the error. Group: account login feedback. |
| `xolotl providers set-key` | `<name>` | Set key / Replace key on provider row | Partial: password input replaces stdin, saved key state and API errors appear. Response includes `catalog_refresh`; this page discards the immediate discovery result, relying on later shared discovery polling. UI hides this action for disabled/no-auth/workload-identity providers, while CLI can submit any name and receive the API's result/rejection. Group: provider credential feedback. |

### Models and request usage

Sources: [models](../../ui/src/pages/models.tsx), [catalog refresh](../../ui/src/components/catalog-refresh.tsx),
[usage](../../ui/src/pages/usage.tsx) and [usage URL state](../../ui/src/lib/usage-view.ts).

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl models` | `--metadata --refresh` | Models catalog and Refresh models | Partial: discovery refresh, provider/model lists, endpoint support, context/max output, input/output/cache prices, source and fetch age are visible. `--metadata` affects `--json` only; its meaningful fields require the same display. Provider discovery errors/counts are shown. Pricing metadata errors become generic "Refresh failed; cached data retained"; underlying `metadata.error` is hidden. No separate pricing attempt/result display exists when the catalog has no rows. Returning after a scheduled pricing refresh shows only the same generic failure warning and per-row last-good fetch age, with no summary of the latest pricing attempt/result. UI rounds every price to two decimal places per Mtok; CLI formats input/output to four decimals and cache prices without that rounding. A $0.005/Mtok cache price appears as $0.01 in the UI. Group: pricing refresh feedback. |
| `xolotl usage` | `--account --client --group-by --model --provider --since` | Usage group tabs, period selector, row drilldown and removable filter chips | Partial: all four groupings and filters, including empty values, reach the same API; response requests/errors/input/output/cached counts and query errors are visible. CLI accepts arbitrary duration (default 24h); UI allows only 24h/7d/30d/90d, even in its URL parser. Filter values can be set in the browser URL, but there are no direct entry controls for values absent from displayed rows. Group: request usage queries. |

Catalog refresh calls the same endpoint in both clients; the server refreshes
metadata with discovery. The CLI prints the underlying pricing error from route
metadata. Browser discovery status is a different result from pricing status;
its "refreshed" time cannot prove a pricing fetch succeeded. Pricing feedback
belongs with the epic's Models pricing refresh task.

### MCP gateway

Source: [MCP page](../../ui/src/pages/mcp.tsx). Selecting a server provides the
`server` argument. File-based definition input maps to form fields for URL,
description, headers, authentication, OAuth client/scopes and call timeout.

| Command | Local options / arguments | UI equivalent | Status, results and error detail |
|---|---|---|---|
| `xolotl mcp servers` | — | MCP server cards | Equal: name, namespace, source, URL, state, tool count, required secret fields/set state, OAuth sign-in reason and last error; query errors are shown. |
| `xolotl mcp put` | `<file>` | Add server / Edit runtime server | Equal: form maps the server definition, local TOML/JSON parsing is replaced by controls. Config-owned definitions remain read-only. Saved status and mutation errors appear; background tool discovery errors appear on the card. |
| `xolotl mcp remove` | `<server>` | Remove runtime server confirmation | Equal: removal, policy-use/config ownership restriction and underlying API error. |
| `xolotl mcp tools` | `<server>` | Show tools | Partial: names, first description line and query errors match normal CLI output; CLI JSON also exposes input schema and full tool definition. UI has no schema/detail inspector. Group: MCP administration. |
| `xolotl mcp refresh` | `<server>` | Refresh tools globally; Refresh on each card | Partial: optional server maps to both controls. Single-server count/error is shown. Global response contains per-server states/errors; UI discards it and toasts "Tool lists refreshed" even if individual refreshes failed. Cards refetch last errors, but do not provide an aggregate mixed outcome. Group: MCP administration. |
| `xolotl mcp set-secret` | `<server> <field>` | Password field and Save for each required header placeholder | Partial: set/unset badges and underlying mutation errors are visible. CLI can name any field; UI exposes only the fields listed in `secret_fields`. Both clients trim the supplied secret. Group: MCP administration. |
| `xolotl mcp unset-secret` | `<server> <field>` | Gap | Gap: no remove-value control sends a null secret field. Removing a header placeholder is not deleting the stored secret. Group: MCP administration. |
| `xolotl mcp login` | `<server>` | Sign in / Sign in again | Partial: authorization URL, callback polling or pasted redirect, completion and OAuth failure reason appear. Callback polling HTTP errors have no visible QueryError; CLI exits with the underlying failure. Group: MCP administration. |
| `xolotl mcp logout` | `<server>` | Sign out confirmation | Partial: deletes OAuth tokens and shows the new sign-in requirement and mutation errors. Sign out is hidden whenever `needs_login` is set, though CLI can still forget existing rejected/expired tokens. Group: MCP administration. |
| `xolotl mcp usage` | `--group-by --since` | Tool calls, last 24 hours | Partial: server/tool/client grouping, calls/errors/average latency match. Optional CLI window defaults to 24h and accepts custom durations; UI fixes 24h. The aggregate query error is not rendered, so a failure can leave an empty table. The entire calls section is hidden when no MCP servers are listed, even when past calls remain available through the CLI. Group: MCP usage queries. |

### Gap groups

These groups provide boundaries for implementation tasks and the UI mockup.
They describe missing behavior and feedback, without deciding a screen layout.

| Group | Capabilities and feedback to account for together |
|---|---|
| Deployment reload | Implemented on Overview: explicit vault reload, loaded policy/key/account/provider counts and underlying failure detail, with catalog discovery shown separately. |
| Key lifecycle | Implemented on Client keys: custom expiry/grace with Never/zero defaults, creation/rotation timestamps and current counters, repeated revocation by ID with distinct outcomes, descriptions during grace, and query errors with retry. |
| Account form inputs | Import label and exact signed 64-bit priority entry, with input validation. |
| Account recovery and usage | Retry availability for any existing account; polling disabled accounts; complete provider usage diagnostics in account details; unsaved-token state on account rows. |
| Account login feedback | HTTP polling errors for subscription login flows and immediate catalog discovery outcome after credential-file import, in Add account. |
| Provider credential feedback | Set-key action availability and immediate discovery outcome on provider account rows. |
| Policy inspection | Complete resolved routes with endpoint types and accounts-read failures; unloaded-policy status fetch errors and deletion controls; saved warning text. |
| MCP administration | Secret field set/removal and value handling; full tool definitions; global refresh mixed outcomes; callback polling errors; forgetting tokens that already need sign-in. |
| Request usage queries | Arbitrary look-back durations and direct filter entry on the Usage page. |
| MCP usage queries | Arbitrary look-back durations and visible aggregate query errors in MCP tool calls; access to past calls when no servers are listed. |
| Pricing refresh feedback | Underlying pricing errors and an explicit pricing attempt/result summary for empty catalogs and return visits after scheduled refreshes; sufficient price precision to distinguish sub-cent per-Mtok values. |

### Maintaining parity

[The coverage check](../../tests/cli_parity.rs) walks clap's built command tree and
compares every declared command and local option/argument with the first two
columns of the rendered command tables above. It derives shared flags from
`Remote`, requires their assessments and parser flag assessments in the shared
options table, and distinguishes same-named local options. Fenced examples,
comments and placeholder verdicts do not satisfy coverage. Generated help is
assessed as a single recursive command. It runs
under the existing CI cargo test job. The check prints the recursive long help
with this command, which is also the inventory's command-listing evidence:

```sh
cargo test --locked --test cli_parity print_recursive_help_listing -- --nocapture
```

The [repository instruction](../../AGENTS.md) applies the parity requirement to
new and changed capabilities, including defaults, accepted values, response
fields and error feedback. A structural check cannot decide whether a browser
workflow conveys the same meaning. That assessment uses the CLI handlers and API
responses, and affected workflows are verified through the standard container
using the browser alone. Existing partial/gap rows are the inventory for that
work; the check's passing result proves coverage, not closure of those gaps.

## Screens

| Screen | Covers | API |
|---|---|---|
| Overview | Vault reload with counts and errors, separate model discovery, OpenBao and SQLite health, stored policies that did not load, pool availability and usage windows, 24 h totals, provider accounts, recent failed requests | `GET /admin/status`, `/admin/accounts`, `/admin/providers`, `/admin/usage`, `/admin/catalog/status`; `POST /admin/reload`, `/admin/catalog/refresh` |
| Accounts | Pool accounts by provider; refresh usage, enable/disable, priority, Retry, replace API key, refresh models, remove; details. Provider accounts: kind, key state, base URL, source; set or replace the key; remove the ones in OpenBao. A **Providers** tab (`?kind=providers`) | `/admin/accounts/...`, `GET /admin/providers`, `POST /admin/providers/<name>/key`, `DELETE /admin/providers/<name>`, `GET /admin/account-kinds` |
| Add account | Built from the account kinds: a login component per OAuth kind, a key form per API-key pool kind, import of a CLI credential file, and one provider account form with a kind selector | `GET /admin/account-kinds`, `/admin/auth/...`, `POST /admin/accounts/import`, `POST /admin/accounts` |
| Policies | A warning listing stored policies that did not load (`unloaded_policies` of `GET /admin/status`). Collapsible sections: scopes, with `key_policies` under the `keys` scope; providers with the accounts of each pool (`accounts`), and a warning under an allowed pool with no accounts checked, when the policy's keys can call providers; MCP access; rate limits in total, per provider and per account, with the policy's consumption. The model list has two modes. Rules: the ordered `rules` (a switch for allow/deny, move up/down, matched and decided counts), a summary of the model switches, the `deny_other_models` row, and aliases (name, a searchable target, fallbacks). Effective models: aliases read-only, and every model of the allowed providers (a switch written to `models`; greyed with the deciding rule when a rule masks it; an override, a rule for the exact `provider/model` at the top, switched in place in the warning colour; inline rename; the winner of a shared name; the state of the accounts the policy uses), with turn on/off all shown models, filter by provider, sort, show/hide denied. Precedence above both | `/admin/policies/...`, `POST /admin/policy-preview`, `GET /admin/status`, `GET /admin/accounts`, `GET /admin/providers` |
| Client keys | Issue, rotate with grace, enable/disable, revoke. Consumption against the policy's rate limits. Shows the vault path and the `bao` command and Vault policy the client needs, never the key | `/admin/keys/...` |
| MCP servers | Upstream MCP servers from the config file and OpenBao: add, edit and remove the latter; header secrets; OAuth sign-in; tools; recent tool calls | `/admin/mcp/...` |
| Models | The catalog with each model's endpoints. `#/providers` redirects here | `GET /admin/catalog/routes`, `POST /admin/catalog/refresh` |
| Usage | Request log grouped by client, account, provider or model. Selecting a row narrows the view to it and groups by the first of model, account, client and provider that is neither shown nor narrowed yet; filters show as removable chips and live in the hash route (`/#/usage?group=model&client=…`). Sessions and tools called, narrowed by the client filter. Link to `telemetry.dashboard_url` | `GET /admin/usage?group_by=…&since_secs=…&client=…&account=…&provider=…&model=…` (an empty filter value matches rows without that key, such as the admin token), `GET /admin/sessions?client=…`, `GET /admin/tools?client=…` |

## Editing

Policy and MCP server editors keep their save and discard or cancel actions in a
sticky bar at the top while the form scrolls. Policy deletion and its key-use
restriction remain in the same bar. Actions wrap on narrow screens.

Buttons use explicit theme text colors in their default and hover states. Disabled
buttons use the existing secondary background and muted text colors, without
fading the whole control.

## Model discovery

Model lists and counts include a shared **Refresh models** control, the age of the
catalog, and each provider's model count or discovery error. The policy model list
uses its selected providers, narrowed to the provider filter when set. Account views use their provider pools: discovery
is shared across accounts of a provider, not performed for one account. A failed
discovery keeps cached and configured static models visible and labels the error.
A successful empty response is shown as **Provider returned no models**.

`GET /admin/catalog/status` returns each provider's `refreshed_at` (Unix seconds
of the last attempt, or null), `models` (including cached and static entries), and
`error` (or null). `POST /admin/catalog/refresh?providers=provider-a,provider-b`
refreshes those providers; omit the query to refresh all providers. Its response
remains the provider-to-model-list map used by the CLI.

Account imports, completed logins, reset, enable/disable, removal, and provider key
changes trigger discovery, as does admin reload. The UI checks discovery status
every 30 seconds and updates model lists after background discovery completes. Manual refresh updates
all open model queries immediately after completion.

## Sign-in

- **OpenID Connect.** The UI signs in through an OpenID Connect provider (built against Pocket ID). Xolotl is a confidential client and uses the authorization code flow with PKCE, all on the server; the browser never holds a provider token. `[ui.oidc]` in the config file names the issuer, client ID, redirect URL and admin group. The client secret is the vault secret `oidc` (field `client_secret`). Without `[ui.oidc]`, nobody can sign in to the UI.
- **Flow.** `GET /admin/session/login` discovers the provider, keeps the `state`, nonce, PKCE verifier and provider metadata in memory for 10 minutes, sets the `xolotl_oidc` cookie to the `state` (`HttpOnly`, `SameSite=Lax` because the callback is a navigation from the provider's site, scoped to the callback path), and redirects to the provider. `GET /admin/session/callback` completes only in the browser that started the sign-in, once per `state`. It exchanges the code, checks the ID token (an asymmetric signature from the provider's keys — RSA, ECDSA or EdDSA, never HMAC — issuer, audience, expiry, nonce), and redirects to `/`. A failed sign-in redirects to `/?signin=<reason>` with one of a fixed set of reasons (`denied`, `expired`, `failed`, `forbidden`, `unavailable`); the details go to the log.
- **Admin group.** Only a user whose `groups` claim contains `admin_group` gets a session. Xolotl requests the `groups` scope and checks the claim itself, so loosening the client's allowed groups in the provider does not widen admin access.
- **Session.** The callback sets `xolotl_session`: `HttpOnly`, `SameSite=Strict`, `Path=/`, 12 hours. Both cookies are `Secure` when `redirect_url` is `https`. The session does not depend on the provider's tokens, which are not kept. `GET /admin/session` returns the CSRF token and the user's name (`preferred_username`, or the subject). `DELETE /admin/session` signs out of xolotl only; the provider session stays.
- **CSRF.** A session authorizes a request only with its CSRF token in `x-csrf-token`, on every method. Browsers never add that header to a cross-site request, and the cookie is `SameSite=Strict` as well.
- **Scope.** A session is an admin principal for `/admin` routes only; it never authorizes the LLM or management endpoints. A request that presents a key is judged by the key alone.
- **Limits.** At most 256 sign-ins in progress are kept; a new one drops the oldest. Starting a sign-in is unauthenticated, so memory stays bounded, and a burst of requests can't block sign-in for the next 10 minutes. Each start makes two requests to the provider (discovery and its keys); provider responses are limited to 1 MiB. One browser has one sign-in in progress: starting a second (another tab) makes the first fail with `expired`, and the second still completes.
- **State.** Sessions and sign-ins in progress live in memory: a restart signs everyone out, and instances don't share them.

### Decisions

- **No password sign-in.** When the provider is down, the CLI with the bootstrap admin token does everything the UI does. A second browser sign-in would be one more auth surface to keep safe for no new capability.
- **Xolotl checks the group.** The provider can also limit a client to groups, but that setting lives outside xolotl and is easy to change without thinking of admin access. Checking `groups` in xolotl keeps the admin boundary in xolotl's own config.
- **Discovery per sign-in.** Sign-ins are rare (sessions last 12 hours), so xolotl discovers the provider on each one instead of caching metadata and keys. That keeps signing-key rotation correct without a cache to expire, and the server starts when the provider is down.
- **Local sign-out.** Signing out of the UI does not end the provider session, which other applications share.

## Build and serving

`ui/` is a Vite + React + TypeScript app with Tailwind CSS v4 and shadcn/ui components (in `ui/src/components/ui`, written from shadcn's source because its registry is not reachable from every build environment). Data comes from TanStack Query; routes are hash routes (`/#/accounts`).

- `pnpm build` writes `ui/dist`. Release builds embed it with `rust-embed`; debug builds read it from disk, so a rebuilt UI shows up after a reload. A binary built without `ui/dist` serves a page saying so.
- The app claims `/`, `/assets/*` and the icon files browsers look for at the root (`favicon.ico`, `favicon.svg`, `apple-touch-icon.png`, `site.webmanifest` and the manifest's icons, from `ui/public`). Every screen is in the URL fragment, so no other path is taken from the `/<provider>/...` passthrough; `assets` (like `ui`) is rejected as a provider account name.
- The UI has a light and a dark theme. It follows the browser's color scheme unless a theme is picked in the sidebar (system, light or dark); the choice is kept in `localStorage`. Theme-dependent colors in `ui/src/index.css` use `light-dark()` pairs; the sidebar is dark in both themes.
- `index.html` is served with `no-cache` and a strict Content-Security-Policy (same-origin scripts, styles, fonts and requests; no framing). Hashed files under `/assets/` are cached for a year; the root icon files for a day. Fonts (IBM Plex) are bundled, so the UI loads nothing from other hosts.
- The emblem and its icons are described in `brand/README.md`.
- The Docker image builds the UI in a Node stage; CI runs `pnpm test` (Vitest on `ui/src` and `ui/test`) and `pnpm build` (which type-checks) in its own job, and the release workflow builds the UI before the binary.

## Recovering an account

Overview attention links open the affected account's details. OAuth accounts show **Re-login needed** and an inline login flow. API-key accounts show **Key rejected** for a 401 or **Refused** for another recorded 4xx; their details show the last error and **Replace key**. Replacement keeps the account ID, label, priority, enabled state and usage history.

Cooldowns show their reason and remaining time on the account row and in details. **Retry** clears the auth failure and cooldown. It permits another attempt with the same credential; it does not reset upstream quota. Disabled accounts stay disabled.

Adding an API-key account, replacing a key and Retry refresh that provider's catalog. **Refresh models** beside **Refresh usage** also runs discovery and reports the model count or discovery error. A discovery error preserves the previous catalog and does not undo a saved key or reset. The policy editor shows each provider’s discovery result beside the model list.
