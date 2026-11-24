# Models pricing container evidence

These fixtures use synthetic credentials, a real disposable OpenBao server, and
Xolotl's normal Dockerfile. Product actions, sign-in and result checks run in the
browser. Only the synthetic registry's failure mode changes outside the browser.
The capture does not stub Xolotl responses or seed browser session state.

Requires Linux, Docker, Node 26, curl, agent-browser, and a Chrome CDP endpoint
on port 19222. A container can supply Chrome's system libraries if the host
lacks them. In that case mount a Chrome installation as `/chrome` in
`mcr.microsoft.com/playwright:v1.56.1-noble`, use host networking, and run
`/chrome/chrome --headless --no-sandbox --remote-debugging-port=19222 about:blank`.

From the repository root, build the normal image and start the fixtures:

```bash
docker build --build-arg TARGETARCH=models-task7 -t xolotl-models-pricing:task7 .
export MODELS_EVIDENCE_DIR=$(mktemp -d /tmp/agent.XXXXXXXX)
node scripts/evidence/models-pricing-fixture.mjs > "$MODELS_EVIDENCE_DIR/fixture.log" 2>&1 &
echo $! > "$MODELS_EVIDENCE_DIR/fixture.pid"
docker run -d --name models-task7-bao --network host \
  -e BAO_DEV_ROOT_TOKEN_ID=models-test-root openbao/openbao:latest \
  server -dev -dev-listen-address=127.0.0.1:18200
```

Wait for `curl -f http://127.0.0.1:18200/v1/sys/health` to succeed, then configure
the isolated test vault. All values below are public synthetic fixture inputs:

```bash
bao_post() {
  curl -fsS -H 'X-Vault-Token: models-test-root' -d "$2" "http://127.0.0.1:18200/v1/$1"
}
bao_post sys/auth/approle '{"type":"approle"}'
bao_post sys/policies/acl/models-test '{"policy":"path \"secret/*\" { capabilities = [\"create\", \"read\", \"update\", \"delete\", \"list\"] }"}'
bao_post auth/approle/role/models-test '{"token_policies":["models-test"]}'
bao_post auth/approle/role/models-test/role-id '{"role_id":"models-test-role"}'
bao_post auth/approle/role/models-test/custom-secret-id '{"secret_id":"models-test-secret"}'
bao_post secret/data/xolotl/admin '{"data":{"token":"models-test-admin"}}'
bao_post secret/data/xolotl/oidc '{"data":{"client_secret":"models-test-oidc"}}'
docker run -d --name models-task7-app --network host \
  -v "$PWD/scripts/evidence/models-pricing.toml:/etc/xolotl/config.toml:ro" \
  xolotl-models-pricing:task7
```

Wait for the page on `http://127.0.0.1:28317`, then capture:

```bash
scripts/evidence/models-pricing-capture.sh
```

The script writes `result.json` as failed before starting and marks it passed
only after its checks complete. It captures manual success, in-progress feedback
and a disabled refresh button, manual HTTP 503 failure, and a different HTTP 502
failure from the real 60-second maintenance timer after navigating away. It
checks `$0.005` precision, retained cache age, and phone page overflow. Output:
`success.png`, `refreshing.png`, `manual-failure.png`, `scheduled-failure.png`,
`phone-failure.png`, a short `models-pricing.webm`, and text/JSON results.

Copy `manual-failure.png` to `docs/user/assets/models-pricing.png`. Generate the
documentation animation from the recording:

```bash
ffmpeg -y -i "$MODELS_EVIDENCE_DIR/models-pricing.webm" \
  -vf 'fps=4,scale=800:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse' \
  docs/user/assets/models-pricing.gif
```

Teardown only the resources created for this run:

```bash
agent-browser --session models-pricing-evidence close
docker rm -f models-task7-app models-task7-bao
kill "$(cat "$MODELS_EVIDENCE_DIR/fixture.pid")"
docker image rm xolotl-models-pricing:task7
```

Remove the optional Chrome container if one was created. Keep the output folder
until its evidence has been published or handed off. The latest attempt/error
is process-local; the successful pricing cache persists in the app's database.
