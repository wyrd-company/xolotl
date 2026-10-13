# Web UI

`xolotl serve` serves an admin web UI at `/`. It is a client of the same `/admin` API the CLI uses, plus a few endpoints added for it.

## Screens

| Screen | Covers | API |
|---|---|---|
| Overview | OpenBao and SQLite health, pool availability and usage windows, 24 h totals, gateway providers, recent failed requests | `GET /admin/status`, `/admin/accounts`, `/admin/providers`, `/admin/usage` |
| Accounts | List by provider; refresh usage, enable/disable, priority, clear cooldown, remove; details | `/admin/accounts/...` |
| Add account | Codex device or browser login, Claude paste login, OpenCode Go key, import of a CLI credential file | `/admin/auth/...`, `POST /admin/accounts/import` |
| Policies | Providers, scopes, allow/deny, precedence, winners for shared names, aliases; a live preview of the names a key sees, with native and translated endpoints | `/admin/policies/...`, `POST /admin/policy-preview` |
| Client keys | Issue, rotate with grace, enable/disable, revoke. Shows the vault path and the `bao` command and Vault policy the client needs, never the key | `/admin/keys/...` |
| Providers & models | Gateway providers from the config file and their key state; set a key; the catalog with each model's endpoints | `GET /admin/providers`, `GET /admin/catalog/routes`, `POST /admin/catalog/refresh` |
| Usage | Request log grouped by client, account, provider or model; link to `telemetry.dashboard_url` | `GET /admin/usage` |

## Sign-in

- **Password.** One admin password. `xolotl ui set-password` hashes it with argon2id on the operator's machine and sends only the hash (`PUT /admin/ui/password`, admin scope). The hash is stored as `ui_password_hash` in the vault `admin` secret, next to the bootstrap token, with a check-and-set write that keeps the secret's other fields. Changing it signs out every session, and a sign-in that was checked against the old password while the change happened gets no session. Because `bao kv put` replaces the whole secret, change the bootstrap token with `bao kv patch`.
- **Session.** `POST /admin/session` checks the password and sets `xolotl_session`: `HttpOnly`, `SameSite=Strict`, `Path=/`, 12 hours, and `Secure` when the request arrived with `X-Forwarded-Proto: https`. The response carries a CSRF token; `GET /admin/session` returns it again after a reload. `DELETE /admin/session` signs out.
- **CSRF.** A session authorizes a request only with its CSRF token in `x-csrf-token`, on every method. Browsers never add that header to a cross-site request, and the cookie is `SameSite=Strict` as well.
- **Scope.** A session is an admin principal for `/admin` routes only; it never authorizes the LLM or management endpoints. A request that presents a key is judged by the key alone.
- **Lockout.** Five failed attempts from one address lock it out for 15 minutes.
- **State.** Sessions and failure counts live in memory: a restart signs everyone out, and instances don't share them.

## Build and serving

`ui/` is a Vite + React + TypeScript app with Tailwind CSS v4 and shadcn/ui components (in `ui/src/components/ui`, written from shadcn's source because its registry is not reachable from every build environment). Data comes from TanStack Query; routes are hash routes (`/#/accounts`).

- `pnpm build` writes `ui/dist`. Release builds embed it with `rust-embed`; debug builds read it from disk, so a rebuilt UI shows up after a reload. A binary built without `ui/dist` serves a page saying so.
- The app claims only `/` and `/assets/*`. Every screen is in the URL fragment, so no other path is taken from the `/<provider>/...` passthrough; `assets` (like `ui`) is rejected as a gateway provider name.
- `index.html` is served with `no-cache` and a strict Content-Security-Policy (same-origin scripts, styles, fonts and requests; no framing). Hashed files under `/assets/` are cached for a year. Fonts (IBM Plex) are bundled, so the UI loads nothing from other hosts.
- The Docker image builds the UI in a Node stage; CI runs `pnpm build` (which type-checks) in its own job, and the release workflow builds the UI before the binary.
