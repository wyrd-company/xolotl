# Changelog

## 0.1.2

### Features

- NATS export: with `[export.nats]`, xolotl publishes each exported exchange to a NATS JetStream stream, and ClickHouse loads the stream with its NATS table engine. Rows published while ClickHouse is down wait in the stream; rows waiting while NATS restarts are held in xolotl's queue and retried until NATS takes them. A row too large for a message has its bodies cut, or is refused and counted. Xolotl checks at startup that the stream exists and takes `xolotl.exchanges.>`. The new NATS export page gives the stream, consumer and table commands, permissions, sizing and alerts. The direct ClickHouse export is unchanged. Building from source needs Rust 1.89.
- Anthropic and OpenAI API accounts: the new provider kinds `anthropic` and `openai` take an API key (personal, project or service account) and serve the same endpoints as Claude and Codex subscription accounts, so a client can use either. An Anthropic account serves `/v1/messages` and `/v1/messages/count_tokens` natively, with the key in `x-api-key` and the subscription OAuth beta flag dropped, so Claude Code requests and their cache breakpoints reach the API unchanged; an OpenAI account serves `/v1/responses` and `/v1/chat/completions` natively. Alias a model to a subscription and an API account to move to the API account when the subscription is at its limit. In the config file, an Anthropic or OpenAI account can authenticate with workload identity federation (`workload_identity`) instead of a key: xolotl exchanges a JWT from a file for a service account access token and renews it before it expires. The Accounts page documents both.
- Provider accounts: a provider is now one more kind of account. Add and remove Vercel, Mistral and OpenAI-compatible providers at runtime, on the web UI's Add account screen or with `xolotl accounts add-provider`, without editing the config file. The Accounts screen lists provider accounts next to pool accounts; the Providers & models screen is now Models. Config-file providers and their stored keys keep working.
- The ClickHouse and NATS exports now cover every kind of account, not only the sessions of subscription accounts: provider accounts (Vercel, Mistral, OpenAI-compatible) and API key accounts are exported too, and so are requests without a session, such as embeddings, OCR and passthroughs. A new `account_kind` column names the kind of the account that served each row. Responses that are not text, such as audio, and passthrough uploads that are not JSON are exported without their bodies. A table made earlier keeps receiving rows without the new column; the ClickHouse and NATS export pages give the `ALTER TABLE` that adds it.

### Fixes

- Link docs pages to each other by file name, and remove the links to design notes, which are not published.
- Codex shows its usage limits when the Codex pool runs out. When every Codex account is cooling down, the 429 now carries the `x-codex-*` usage headers of the account that comes back first. A quota error at the start of a stream keeps the account's usage headers and uses the error shape Codex reads. Before, Codex showed no limits at that point. The Codex client guide explains where Codex gets its limits through xolotl, and why `/status` has none before the first turn.
- Give the web UI steps first on each task page (accounts, policies and keys, MCP servers, gateway provider keys), and the CLI commands after them. The web UI page now sits under Set up.
- The configuration and client docs now say that every endpoint, including the T3 Code hubs endpoints under `/v0/management/`, is served on the one `server.listen` address, and that a reverse proxy must forward that path too.
- Document how to add an OpenCode service-account key as an OpenCode Go account.

## 0.1.1

### Features

- Warn when a policy allows a subscription provider with no accounts. Such a provider looks enabled, but its requests get a 403. `xolotl policies put` and `PUT /admin/policies/{name}` still save the policy and now name each such provider, and the web UI's policy editor shows the same warning. `xolotl policies view` and `GET /admin/policies/{name}/view` list the accounts each subscription provider can use, and say `no accounts` where there are none.
- Let a tool issue client keys without the admin token. A policy with the new `keys` scope lists its `key_policies`; its keys can issue, revoke, disable and list keys under those policies, and reach nothing else. Each change to a key logs the key that acted.
  
  Issuing and revoking are now safe to repeat. `POST /admin/keys` on a name with a live key answers 200 with `status: exists` and no key, where it was a 400, and a new key answers 201 where it was 200, with `status: created`. `DELETE /admin/keys/<policy>/<name>` on a key that is missing or already revoked answers 200, where it was a 404, and its `status` says what it found. Vault failures on both answer 500, where issuing gave a 400 and revoking a 404.
  
  Upgrade note: a script that treats the 400 on a repeated issue, or the 404 on a repeated revoke, as a signal must read `status` instead.
- Track each conversation and keep long ones on the account that holds their prompt cache. Xolotl now identifies the session of every LLM request and records its size against the model's context window, its peak, its turns and its cache hit ratio. They are in the request log, in new metrics, at `GET /admin/sessions` and on the Usage screen. The tools each key calls are at `GET /admin/tools` and on the same screen.
  
  A session whose prompt reaches `sessions.lock_tokens` (default 60 000) now stays on its account and model, past the soft limit and ahead of alias order. The lock breaks on a cooldown, a lost account, a policy or limit change, or `subscription.max_attempts` failed requests in a row.
  
  Upgrade note: a locked session's client now sees a 5xx or a transport error from its account, up to that limit, where it used to fail over. Set `lock_tokens = 0` to keep the old failover.

### Fixes

- Give the xolotl pages on wyrd.foo their own social image, in place of the site-wide one.
- Show the Homebrew install on the xolotl product page, and use the current version in the binary install example.
- Say what happens to a code mode tool call that the code does not await: the gateway stops waiting for it when the code returns, and it may never start or may still run on the upstream.
- Publish the public docs from docs/user in the wyrd.foo format.
- Ship the binary for Linux arm64 and macOS arm64 as well as Linux x86_64, and install it with `brew install wyrd-company/tools/xolotl`.
- Keep policies from before 0.4.0 working after an upgrade. Upgrading to 0.4.0 or 0.5.0 disabled every stored policy with `allow_models` or `deny_models`, and its keys got `401 invalid API key`. Such a policy now migrates to `rules` and `accounts` with the same effect when it loads. A stored policy that does not load now shows in `xolotl policies list`, `policies get`, `GET /admin/status` and the web UI, and a valid key refused because of it is logged with its client and policy.
  
  Upgrade note: a policy from before 0.4.0 with neither `allow_models` nor `deny_models` can't be told apart from a current policy that uses no accounts, so it is not migrated. Its keys get a 403 from each subscription provider. To restore access, add `accounts` to the policy, for example `"accounts": {"claude": ["*"]}`, and save it with `xolotl policies put`.
