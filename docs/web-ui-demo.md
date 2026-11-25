# Web UI demo and screenshot recipe

From the repository root on Linux, run the demo with Docker, Python 3, curl
and OpenSSL installed.

```sh
docs/web-ui-demo.sh
```

The script builds the repository's Dockerfile and starts xolotl, a dev
OpenBao server and a throwaway OpenID Connect provider. Open the loopback
URL it prints and select **Sign in**. The identity provider signs you in as
**Demo operator**. The demo has a synthetic Codex subscription, an
OpenAI-compatible local provider, the `devcontainer` policy, a client key,
and an MCP server with a header secret. It makes one model request and one
tool call through xolotl to populate usage.

Press **Ctrl+C** to stop the demo. The script removes this run's containers,
database volume, temporary files and build tag. All three services listen on
loopback. The identity provider grants the demo admin identity to anyone who
selects **Sign in**, so only run it on a trusted machine. Every credential
belongs to the disposable fixture.

## Regenerate the screenshots

Install [agent-browser](https://agent-browser.dev/) and its Chromium browser.
Pass the capture command to the demo so it starts with fresh data, runs the
capture and removes the demo when the command exits.

```sh
capture_dir=$(mktemp -d)
docs/web-ui-demo.sh docs/web-ui-capture.sh "$capture_dir"
```

The script checks that each screen has loaded the state it depicts before
capturing. Review the captures, then copy the ones you want to update into
`docs/user/assets/`. Dates, durations and the fixture's loopback port can vary
from run to run.

| PNG | State |
|---|---|
| `deployment-reload.png` | Overview after a successful reload from OpenBao |
| `accounts-list.png` | Account list with the sample subscription and provider |
| `add-account.png` | Add account with subscription login and import controls |
| `add-provider-account.png` | Provider account form |
| `new-policy.png` | New policy with the name, description and provider selected |
| `policy-no-accounts.png` | Codex selected with its account unchecked |
| `client-key-issued.png` | Issued client key with an expiry and OpenBao retrieval instructions |
| `client-key-rotate.png` | Rotation dialog with a custom grace period |
| `mcp-header-secret.png` | Upstream MCP server with a configured header secret and tool list |
| `mcp-tool-calls.png` | Tool calls grouped by tool over 36 hours |
| `models-catalog.png` | Model catalog with provider discovery and sample pricing |

For a repeat capture, set `XOLOTL_DEMO_IMAGE` to a prebuilt local tag from this
checkout. If Chromium runs separately on the same machine, set
`XOLOTL_CAPTURE_CDP_PORT` to its loopback debugging port and use a disposable
browser profile. The capture script closes its browser session when it exits.

The pricing failure, account, and request usage recordings each have their own
state-specific fixtures. See [model pricing](../scripts/evidence/README.md),
[account workflows](../tests/browser/account-form/README.md),
[provider credentials](../tests/browser/provider-credentials/README.md) and
[request usage](../tests/browser/request-usage/README.md).
