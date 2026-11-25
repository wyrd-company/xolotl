#!/usr/bin/env bash
# Run a disposable, loopback-only demo using Linux Docker host networking.
# With no command, serve until Ctrl+C. See docs/web-ui-demo.md.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
demo_dir=$(mktemp -d /tmp/xolotl-web-ui.XXXXXX)
run_name="xolotl-demo-${demo_dir##*.}"
app="$run_name-app"
vault="$run_name-bao"
fixture="$run_name-fixture"
image=${XOLOTL_DEMO_IMAGE:-"xolotl-web-ui-demo:${demo_dir##*.}"}
owns_image=false

cleanup() {
  docker rm -fv "$app" "$fixture" "$vault" >/dev/null 2>&1 || true
  if "$owns_image"; then
    docker image rm "$image" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$demo_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Keep the sockets open until all three distinct ports have been chosen.
read -r app_port vault_port fixture_port < <(python3 - <<'PY'
import socket
sockets = [socket.socket() for _ in range(3)]
for sock in sockets:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in sockets))
PY
)
export XOLOTL_DEMO_URL="http://127.0.0.1:$app_port"
fixture_url="http://127.0.0.1:$fixture_port"
admin_token=$(openssl rand -hex 24)
client_secret=$(openssl rand -hex 24)

if [[ -z ${XOLOTL_DEMO_IMAGE:-} ]]; then
  owns_image=true
  docker build --tag "$image" "$repo"
fi

wait_for() {
  local container=$1 url=$2
  for _ in {1..60}; do
    if curl --noproxy '*' --max-time 1 -fs "$url" >/dev/null; then
      return
    fi
    sleep 0.5
  done
  docker logs "$container" >&2
  printf 'Demo could not reach %s\n' "$url" >&2
  return 1
}

docker run -d --name "$vault" --network host \
  -e "BAO_DEV_LISTEN_ADDRESS=127.0.0.1:$vault_port" \
  -e BAO_DEV_ROOT_TOKEN_ID=xolotl-demo-root \
  openbao/openbao:2.7.2 server -dev >/dev/null
wait_for "$vault" "http://127.0.0.1:$vault_port/v1/sys/health"

demo_bao() {
  docker exec -i -e "BAO_ADDR=http://127.0.0.1:$vault_port" \
    -e BAO_TOKEN=xolotl-demo-root "$vault" bao "$@"
}
demo_bao auth enable approle >/dev/null
demo_bao policy write xolotl - >/dev/null <<'HCL'
path "secret/data/xolotl/*"     { capabilities = ["create", "read", "update", "delete"] }
path "secret/metadata/xolotl/*" { capabilities = ["list", "read", "delete"] }
path "secret/destroy/xolotl/*"  { capabilities = ["update"] }
HCL
demo_bao write auth/approle/role/xolotl token_policies=xolotl >/dev/null
role=$(demo_bao read -field=role_id auth/approle/role/xolotl/role-id)
secret=$(demo_bao write -f -field=secret_id auth/approle/role/xolotl/secret-id)
demo_bao kv put secret/xolotl/admin token="$admin_token" >/dev/null
demo_bao kv put secret/xolotl/oidc client_secret="$client_secret" >/dev/null

docker run -d --name "$fixture" --network host \
  -e "DEMO_ISSUER=$fixture_url" -e "DEMO_APP_URL=$XOLOTL_DEMO_URL" \
  -e "DEMO_OIDC_SECRET=$client_secret" \
  -v "$repo/docs/web-ui-demo.mjs:/fixture.mjs:ro" \
  node:26-slim node /fixture.mjs >/dev/null
wait_for "$fixture" "$fixture_url/.well-known/openid-configuration"

cat >"$demo_dir/config.toml" <<TOML
[server]
listen = "127.0.0.1:$app_port"

[vault]
address = "http://127.0.0.1:$vault_port"

[vault.approle]
role_id = "$role"
secret_id = "$secret"

[ui.oidc]
issuer = "$fixture_url"
client_id = "xolotl-demo"
redirect_url = "$XOLOTL_DEMO_URL/admin/session/callback"
admin_group = "demo-admins"

[catalog]
metadata_url = "$fixture_url/registry"

[subscription]
usage_poll_secs = 0

[subscription.endpoints]
codex_backend = "$fixture_url"
TOML
# The runtime container runs as nonroot. The containing directory stays private.
chmod 644 "$demo_dir/config.toml"

docker run -d --name "$app" --network host \
  -v "$demo_dir/config.toml:/etc/xolotl/config.toml:ro" "$image" >/dev/null
wait_for "$app" "$XOLOTL_DEMO_URL/readyz"

admin() {
  curl --noproxy '*' --max-time 30 --fail-with-body -sS -X "$1" \
    -H "Authorization: Bearer $admin_token" -H 'Content-Type: application/json' \
    --data "$3" "$XOLOTL_DEMO_URL/admin/$2" >/dev/null
}
admin POST accounts/import '{
  "provider":"codex", "label":"Team subscription",
  "data":{"tokens":{"access_token":"demo-codex-only","account_id":"demo-team"}}
}'
admin POST accounts "{
  \"kind\":\"openai-compatible\", \"name\":\"local\",
  \"base_url\":\"$fixture_url/v1\", \"no_auth\":true
}"
admin POST mcp/servers "{
  \"name\":\"project-notes\", \"url\":\"$fixture_url/mcp\",
  \"description\":\"Find notes from the demo project\",
  \"headers\":{\"Authorization\":\"Bearer {token}\"}
}"
admin PUT mcp/servers/project-notes/secret '{"fields":{"token":"demo-notes-only"}}'
admin POST mcp/servers/project-notes/refresh '{}'
admin PUT policies/devcontainer '{
  "name":"devcontainer", "description":"Coding and project notes",
  "providers":["local","codex"], "accounts":{"codex":["*"]},
  "scopes":["proxy","mcp"], "mcp_servers":["project-notes"]
}'
admin POST keys '{"policy":"devcontainer","name":"workstation","description":"Demo workstation"}'

client_key=$(demo_bao kv get -field=api_key secret/xolotl/keys/devcontainer/workstation)
demo_request() {
  curl --noproxy '*' --max-time 30 --fail-with-body -sS \
    -H "Authorization: Bearer $client_key" -H 'Content-Type: application/json' "$@"
}
demo_request --data '{"model":"qwen3-coder","messages":[{"role":"user","content":"Summarize the demo."}]}' \
  "$XOLOTL_DEMO_URL/v1/chat/completions" >/dev/null
demo_request -H 'Accept: application/json, text/event-stream' -H 'MCP-Protocol-Version: 2025-11-25' \
  --data '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"execute","arguments":{"code":"return await project_notes.find_notes({title: \"Launch\"});"}}}' \
  "$XOLOTL_DEMO_URL/mcp" | python3 -c '
import json, sys
reply = json.load(sys.stdin)
if reply.get("result", {}).get("isError") is not False:
    sys.exit("Could not seed MCP tool usage: " + str(reply))
'

printf 'Open %s and select Sign in as the demo operator.\n' "$XOLOTL_DEMO_URL"
if (( $# )); then
  "$@"
else
  printf 'Leave this shell running while you use the demo. Press Ctrl+C to stop.\n'
  docker wait "$app"
fi
