#!/usr/bin/env bash
# Starts a throwaway OpenBao dev server (Docker) and a debug xolotl for
# docs/demo.tape. Source it from the repository root after `cargo build`:
#
#   source docs/demo-env.sh        # set up
#   xolotl_demo_down               # tear down
#
# Nothing here is a production setup: the OpenBao dev server keeps its data in
# memory and uses a fixed root token.
set -euo pipefail

XOLOTL_DEMO_DIR=/tmp/xolotl-demo
XOLOTL_DEMO_BAO=xolotl-demo-bao
XOLOTL_DEMO_BAO_PORT=18200
XOLOTL_DEMO_PORT=18317

bao() {
  docker exec -i -e BAO_ADDR=http://127.0.0.1:8200 -e BAO_TOKEN=demo-root "$XOLOTL_DEMO_BAO" bao "$@"
}

xolotl_demo_down() {
  [ -f "$XOLOTL_DEMO_DIR/xolotl.pid" ] && kill "$(cat "$XOLOTL_DEMO_DIR/xolotl.pid")" 2>/dev/null || true
  docker rm -f "$XOLOTL_DEMO_BAO" >/dev/null 2>&1 || true
  rm -rf "$XOLOTL_DEMO_DIR"
}

xolotl_demo_down
mkdir -p "$XOLOTL_DEMO_DIR"

docker run -d --name "$XOLOTL_DEMO_BAO" -p "127.0.0.1:$XOLOTL_DEMO_BAO_PORT:8200" \
  openbao/openbao:latest server -dev -dev-root-token-id=demo-root >/dev/null
until curl -sf "http://127.0.0.1:$XOLOTL_DEMO_BAO_PORT/v1/sys/health" >/dev/null; do sleep 0.5; done

bao auth enable approle >/dev/null
bao policy write xolotl - >/dev/null <<'HCL'
path "secret/data/xolotl/*"     { capabilities = ["create", "read", "update", "delete"] }
path "secret/metadata/xolotl/*" { capabilities = ["list", "read", "delete"] }
path "secret/destroy/xolotl/*"  { capabilities = ["update"] }
HCL
bao write auth/approle/role/xolotl token_policies=xolotl >/dev/null
XOLOTL_VAULT_ROLE_ID=$(bao read -field=role_id auth/approle/role/xolotl/role-id)
XOLOTL_VAULT_SECRET_ID=$(bao write -f -field=secret_id auth/approle/role/xolotl/secret-id)
XOLOTL_ADMIN_TOKEN=$(openssl rand -hex 24)
bao kv put secret/xolotl/admin token="$XOLOTL_ADMIN_TOKEN" >/dev/null
export XOLOTL_VAULT_ROLE_ID XOLOTL_VAULT_SECRET_ID XOLOTL_ADMIN_TOKEN

# One OpenAI-compatible provider (for example llama.cpp). Its models are
# static, so the demo needs no running model server.
cat >"$XOLOTL_DEMO_DIR/config.toml" <<TOML
[server]
listen = "127.0.0.1:$XOLOTL_DEMO_PORT"

[database]
path = "$XOLOTL_DEMO_DIR/xolotl.db"

[vault]
address = "http://127.0.0.1:$XOLOTL_DEMO_BAO_PORT"

[gateway.providers.local]
kind = "openai-compatible"
base_url = "http://127.0.0.1:9"
no_auth = true

[catalog.static_models]
local = ["qwen3-coder", "gemma-4-12b"]
TOML

cat >"$XOLOTL_DEMO_DIR/agents.toml" <<'TOML'
name = "agents"
description = "Coding agents"
providers = ["local"]

[aliases]
fast = "local/gemma-4-12b"
TOML

target/debug/xolotl serve --config "$XOLOTL_DEMO_DIR/config.toml" >"$XOLOTL_DEMO_DIR/server.log" 2>&1 &
echo $! >"$XOLOTL_DEMO_DIR/xolotl.pid"
export XOLOTL_URL="http://127.0.0.1:$XOLOTL_DEMO_PORT"
for _ in $(seq 60); do curl -sf "$XOLOTL_URL/readyz" >/dev/null && break; sleep 0.5; done
curl -sf "$XOLOTL_URL/readyz" >/dev/null || { cat "$XOLOTL_DEMO_DIR/server.log"; return 1 2>/dev/null || exit 1; }

export PATH="$PWD/target/debug:$PATH"
set +euo pipefail
