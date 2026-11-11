# Accounts and account kinds

Every upstream credential xolotl holds is an **account** of one **kind**. `src/accounts.rs` is the registry of kinds. The admin API serves it at `GET /admin/account-kinds`, and the web UI's **Add account** screen is built from it.

## Placement

A kind's placement decides where its accounts serve requests.

| Placement | Kinds | Provider name | Accounts per provider | Served by |
|---|---|---|---|---|
| `pool` | `codex`, `claude`, `grok`, `opencode-go` | Fixed: the kind's provider | Many: selection, failover, cooldowns | `src/subscription/` ([Subscription accounts](subscriptions.md)) |
| `provider` | `anthropic`, `openai`, `vercel`, `mistral`, `openai-compatible` | The account's name, chosen by the operator | One: the account is the provider | `src/gateway.rs` |

Policies, the catalog and routing see providers, not accounts. A pool is one provider; a provider account is one provider.

### Anthropic and OpenAI API accounts

`anthropic` and `openai` are provider kinds, not pools:

- **Spending is a policy decision.** An API account bills per token; a subscription does not. As a provider of its own, an API account is allowed, denied, ranked and aliased by policies apart from the subscription that serves the same models. In a pool, the pool's selection would spend API money whenever the subscriptions were busy.
- **Each key has its own scope.** An unscoped Anthropic key needs `anthropic-workspace-id`; an OpenAI key may need `OpenAI-Project`. A provider account carries its own headers and base URL; a pool account has neither.
- **The names stay free.** A pool takes a fixed provider name, reserved in `RESERVED_PROVIDER_NAMES`. `anthropic` and `openai` are likely names of existing config-file providers, which a reservation would refuse.

Failover between a subscription and an API account, or between several API accounts, is an alias with several targets. A 429 or 5xx moves the request to the next target.

The gateway prepares Anthropic requests as a Claude Code client expects: the key goes in `x-api-key`, `anthropic-version` is added when absent, the subscription's OAuth beta flag is dropped, and a body `betas` list moves to the `anthropic-beta` header. Bodies are otherwise unchanged, so cache breakpoints reach the API as the client set them.

### Service accounts

Both vendors attach API access to a service account in two ways:

- **A service account key** is an API key. It needs nothing beyond the `api-key`/`provider` method.
- **Workload identity federation** exchanges a JWT from the host's identity provider for a short-lived access token: RFC 7523 `jwt-bearer` at `https://api.anthropic.com/v1/oauth/token`, RFC 8693 token exchange at `https://auth.openai.com/oauth/token`. Neither returns a refresh token; renewal is another exchange.

Workload identity is `workload_identity` on a config-file provider. The gateway reads the JWT file on every exchange, holds the access token until 5 minutes before expiry (80% of a shorter lifetime), and drops it when a 401 refuses that token; a 401 for an older token leaves a newer one. Key reloads keep it. A per-provider lock, held in a slot that is never replaced, makes concurrent requests wait for one exchange, because an identity token with a `jti` can be exchanged once. Runtime accounts refuse `workload_identity`: setting it through the admin API would let an operator make the server read any file and send it to a token endpoint. OpenAI's X.509 certificate federation is not supported.

## Add methods

Each kind lists how an operator adds an account of it, in the order the web UI offers them.

| Method | Admin API | Web UI form |
|---|---|---|
| `device-login` | `POST /admin/auth/<kind>/start` with `device: true`; the server polls | The kind's login component (`LOGIN_FORMS` in `ui/src/pages/add-account.tsx`) |
| `browser-login` | `POST /admin/auth/<kind>/start`, then `POST /admin/auth/flows/<id>/complete` with the pasted code | The kind's login component |
| `import` | `POST /admin/accounts/import` with a CLI credential file | **Import a login**, for every kind with this method |
| `api-key` | `POST /admin/accounts` with `kind`, `api_key`, `label` | A generic key form, for every pool kind with this method |
| `provider` | `POST /admin/accounts` with `kind`, `name`, `api_key`, `base_url`, `no_auth`, `headers` | **Provider account**: one form with a kind selector, for every provider kind |

`POST /admin/accounts` is the add endpoint for every method that is one request. OAuth kinds log in through `/admin/auth`.

## Provider accounts

Provider accounts come from two places, keyed by name:

- **Config file:** `[gateway.providers.<name>]`. Read-only at runtime, except for the key. The key is at `providers/<name>` (`{ api_key }`), or at the entry's `secret`. An `anthropic` or `openai` entry can authenticate with `workload_identity` instead (see [Service accounts](#service-accounts)).
- **OpenBao:** added with `POST /admin/accounts`, stored at `providers/<name>` as one secret: the provider fields (`kind`, `base_url`, `no_auth`, `headers`, `enabled`) and `api_key`. A secret at `providers/<name>` without `kind` is a config-file provider's key, not an account.

Rules:

- A name the config file uses is refused, and a vault account with such a name is skipped with a warning. So is an invalid one.
- Runtime accounts get stricter checks than the config file: an `http(s)` base URL, valid header names and values, and no `secret`. The config file keeps its rules so existing files load.
- Adding an account whose `providers/<name>` secret already holds a key, such as a provider moved out of the config file, keeps that key.
- Writes and removals use check-and-set, so two instances can't overwrite each other's change. A key set for an account another instance removed is refused.
- `PUT /admin/providers/<name>` changes an account and keeps its key when none is given; an account that needs a key must keep one. `POST /admin/providers/<name>/key` sets the key of either source and keeps the secret's other fields.
- `DELETE /admin/providers/<name>` empties the secret with check-and-set and destroys the versions that held the key. The empty secret is not an account, and adding the name again reuses it. Removal is refused for config-file providers and while a policy names the provider in `providers`, `precedence` or `winners`.
- Changes apply at once. Other instances pick them up in the 60-second sync. A changed or removed account drops its cached key, and a vault read already in flight can't cache a key that a change replaced. A request to an account in OpenBao takes its base URL, headers and key from one read of the secret, and a cached key is used only with the config it was read with, so a key never goes to another revision's endpoint.
- The catalog follows: a removed provider's models leave it, also when a refresh was running, and a new provider's models are discovered when it is saved.

## Adding a kind

1. **Registry:** add a variant to `AccountKind` in `src/accounts.rs` and its row in `AccountKind::info` (label, description, placement, add methods, default base URL). Add it to `AccountKind::ALL`, and map it in `sub_provider` or `provider_kind`. The tests there check that each kind has one placement and uses the config name.
2. **Serving:**
   - A **pool kind** with a new provider name needs a `SubProvider` variant and a module in `src/subscription/` (request preparation, failure classification, usage), plus its name in `RESERVED_PROVIDER_NAMES`. A new credential type for an existing pool, such as a service account, is a new `AddMethod` for that kind and a new way to obtain the `Credential`.
   - A **provider kind** needs a `ProviderKind` variant in `src/config.rs`, its default base URL, and any request rewriting in `src/gateway.rs` (like `normalize_mistral`). Wire formats for the catalog are in `ModelCatalog::wires`.
   - Either way, the requests go through `Accounting`, built with `with_capture` and `with_target`, so that the request log and the [exchange export](export.md) cover them. The kind's `as_str` is its `account_kind` in the export.
3. **Adding:** the existing `api-key` and `provider` methods need no new route. `add_account` in `src/server/admin.rs` dispatches on the kind. A new method needs a route and an `AddMethod` variant.
4. **Web UI:** `api-key` and `provider` kinds appear on **Add account** from the registry. An OAuth kind needs a component in `LOGIN_FORMS`; a new method needs a form. `AccountKind` and `ProviderKind` in `ui/src/lib/api.ts` list the names.
5. **CLI:** `ProviderKindArg` in `src/cli.rs` lists provider kinds for `xolotl accounts add-provider`.
6. **Docs:** the kind table in `docs/user/accounts.md`.
