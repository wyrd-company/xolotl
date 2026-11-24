# Request usage browser checks and documentation captures

These fixtures use only synthetic data. They build the repository Dockerfile
and exercise its embedded UI through an OIDC browser session. They require Docker,
Node, Python 3 and (for the documentation GIF) FFmpeg. Ports
34200, 34317, 34444 and 34445 must be free. Each fixture run starts with a new vault
and SQLite volume. Do not point setup or seed at another deployment.

From the repository root, copy the fixtures to a disposable directory. Record
resources in your ledger and remove them when finished.

```bash
capture_dir=$(mktemp -d /tmp/agent.XXXXXXXX)
cp tests/browser/request-usage/* "$capture_dir/"
docker build -t xolotl-request-usage:acceptance .
docker run -d --name xolotl-request-vault --network host \
  -e BAO_DEV_ROOT_TOKEN_ID=fixture-root \
  -e BAO_DEV_LISTEN_ADDRESS=127.0.0.1:34200 openbao/openbao:latest server -dev
node "$capture_dir/fixture.mjs" > "$capture_dir/oidc.log" 2>&1 &
oidc_pid=$!
python3 "$capture_dir/vault-proxy.py" > "$capture_dir/proxy.log" 2>&1 &
proxy_pid=$!
node "$capture_dir/setup.mjs"
docker run -d --name xolotl-request-app --network host \
  -v "$capture_dir/config.toml:/etc/xolotl/config.toml:ro" xolotl-request-usage:acceptance
docker run -d --name xolotl-request-browser --network host --ipc=host \
  --volumes-from xolotl-request-app -v "$capture_dir:/evidence" \
  mcr.microsoft.com/playwright:v1.58.2-noble tail -f /dev/null
docker exec xolotl-request-browser npm install --prefix /runner playwright@1.58.2 @axe-core/playwright
docker exec xolotl-request-browser node /evidence/signin.cjs
docker exec xolotl-request-browser python3 /evidence/seed.py
docker exec -e XOLOTL_COMMIT="$(git rev-parse HEAD)" xolotl-request-browser node /evidence/acceptance.cjs
docker exec xolotl-request-browser node /evidence/capture.cjs
ffmpeg -y -i "$capture_dir/usage-queries.webm" \
  -vf 'fps=6,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse' \
  "$capture_dir/usage-queries.gif"
```

Wait for the vault's startup message before setup and the app's HTTP listener
before signin. `result.json` starts failed and changes to passed only after every
assertion finishes. It records the tested commit. Browser route faults simulate
only the aggregate query failure; success, filtering, empty values, retry and
look-back boundaries use the container API and seeded SQLite rows. `axe.json`
contains the main-page WCAG A/AA scan. Screenshots show desktop, phone, invalid
input and query failure. The script also checks keyboard operation at phone width.

`filters-desktop.png` regenerates `docs/user/assets/usage-sessions.png`.
`usage-queries.gif` regenerates `docs/user/assets/usage-queries.gif`. The GIF shows
applying a custom window and client filter, then correcting an invalid duration.
Copy these outputs to the documentation assets when intentionally updating them.

Keep `session.json` local; it contains an ephemeral session cookie. Publish only
synthetic screenshots, GIF and the result report. Cleanup after saving evidence:

```bash
docker rm -fv xolotl-request-browser xolotl-request-app
docker rm -f xolotl-request-vault
docker image rm xolotl-request-usage:acceptance
kill "$oidc_pid" "$proxy_pid"
rm -rf "$capture_dir"
```
