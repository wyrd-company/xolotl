# Provider credential browser acceptance

Build the standard runtime image from the repository root. The fixture uses
synthetic keys, a local upstream, dev OpenBao, and the account-form OIDC fixture
on separate ports. It requires Docker with host networking, Node, and FFmpeg
for the documentation recording.

```sh
fixture_dir=$(mktemp -d /tmp/agent.XXXXXXXX)
sed 's/35444/36444/g' tests/browser/account-form/fixture.mjs > "$fixture_dir/oidc.mjs"
sed -e 's/35200/36200/g' -e 's/35445/36200/g' -e 's/35317/36317/g' \
  -e 's/35444/36444/g' tests/browser/account-form/setup.mjs > "$fixture_dir/setup.mjs"
cp tests/browser/provider-credentials/acceptance.cjs "$fixture_dir/"

docker build -t xolotl-provider-credentials:acceptance .
docker run -d --name xolotl-provider-credentials-vault --network host \
  -e BAO_DEV_ROOT_TOKEN_ID=fixture-root -e BAO_DEV_LISTEN_ADDRESS=127.0.0.1:36200 \
  openbao/openbao:latest server -dev
node "$fixture_dir/oidc.mjs" &
oidc_pid=$!
node tests/browser/provider-credentials/upstream.mjs &
upstream_pid=$!
```

After OpenBao is listening on port 36200, seed the fixture and start Xolotl.

```sh
ACCOUNT_FORM_FIXTURE_DIR="$fixture_dir" node "$fixture_dir/setup.mjs"
cat tests/browser/provider-credentials/providers.toml >> "$fixture_dir/config.toml"
docker run -d --name xolotl-provider-credentials-app --network host \
  -v "$fixture_dir/config.toml:/etc/xolotl/config.toml:ro" xolotl-provider-credentials:acceptance
```

Once Xolotl is listening on port 36317, run Chromium against the container.
The script signs in using the browser and performs every key save and model
refresh through the dialog.

```sh
docker run --rm --network host --ipc=host -v "$fixture_dir:/evidence" -w /runner \
  -e XOLOTL_TEST_HEAD="$(git rev-parse HEAD)" mcr.microsoft.com/playwright:v1.58.2-noble \
  sh -c 'npm install --ignore-scripts --no-package-lock playwright@1.58.2 @axe-core/playwright@4.14.1 &&
    NODE_PATH=/runner/node_modules node /evidence/acceptance.cjs'
```

Read `acceptance.json` for the completed results. The run checks initial save,
the password input and pending state, replacement with a real upstream 503 and
recovery, workload identity rejection, disabled and no-auth saves, and
accessibility of the discovery warning and rejection dialogs. Start with a
fresh vault for another run.

Copy `provider-key-discovery-error.png` to `docs/user/assets/` and convert the
recorded workflow for the account guide.

```sh
ffmpeg -y -i "$fixture_dir/provider-key-workflow.webm" \
  -vf 'fps=5,scale=780:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b]paletteuse' \
  docs/user/assets/provider-key-workflow.gif
```

Remove the fixture containers and stop the two Node processes when finished.

```sh
docker rm -fv xolotl-provider-credentials-app xolotl-provider-credentials-vault
docker image rm xolotl-provider-credentials:acceptance
kill "$oidc_pid" "$upstream_pid"
rm -rf -- "$fixture_dir"
```
