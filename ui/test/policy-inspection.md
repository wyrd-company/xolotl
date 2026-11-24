# Policy inspection browser acceptance

`policy-inspection.browser.cjs` checks the production UI in the standard Xolotl
container, with synthetic OIDC sign-in and an isolated OpenBao fixture. Product
mutations use browser controls. Read failures are injected into the browser's
responses; the key-use deletion refusal comes from the real server.

Seed the fixture before starting Xolotl:

- Configure static `fixture-model` entries for both `codex` and `claude`.
- Store policy `routes` with providers `codex` and `claude`, scope `proxy`,
  `accounts.codex = ["*"]` and alias `fast = ["codex/fixture-model",
  "claude/fixture-model"]`. No subscription accounts are needed.
- Store policies `broken` and `blocked` with `name`, empty providers,
  scope `management`, and an unknown field so they cannot load.
- Store an active synthetic key `blocked/client` under `blocked` with
  `client_id`, `policy`, `name` and `api_key`. Give `broken` no keys.
- Sign in with the fixture's OIDC administrator and save Playwright storage
  state to a local file. Keep session material private.

With Playwright installed in the browser container, run:

```sh
XOLOTL_URL=http://localhost:41317 \
SESSION_FILE=/evidence/session.json \
EVIDENCE_DIR=/evidence \
node policy-inspection.browser.cjs
```

`PLAYWRIGHT_MODULE` optionally names a module path when Playwright is installed
outside the script directory. The output `acceptance.json` starts failed and
becomes passed only after every assertion completes. It records each completed
check and runtime errors. Screenshots include resolved routes, status/account
failures, rejected deletion and save warnings. `policy-routes.png` regenerates
`docs/user/assets/policy-routes.png` at a 1440px desktop viewport.

The run deletes `broken` and creates `browser-hub` and `browser-warned`. Recreate
the isolated app/vault and seed the inputs before repeating the complete run.
Do not reset another task's fixture.
