#!/usr/bin/env bash
# Starts a throwaway OpenBao dev server (Docker) and a debug xolotl for
# docs/demo.tape. Source it in an interactive bash from the repository root,
# after `cargo build`:
#
#   source docs/demo-env.sh        # set up; cd "$XOLOTL_DEMO_DIR" for the files
#   xolotl_demo_down               # tear down (also runs when the shell exits)
#
# Each run owns a fresh temporary directory, a container labelled with that
# directory, and a server process that is a child of this shell. Teardown
# touches only those, so a second run never stops the first.
#
# Nothing here is a production setup: the OpenBao dev server keeps its data in
# memory and uses a fixed root token.

bao() {
  docker exec -i -e BAO_ADDR=http://127.0.0.1:8200 -e BAO_TOKEN=demo-root "$XOLOTL_DEMO_BAO" bao "$@"
}

xolotl_demo_down() {
  # The server: only the child this shell started, and only while it still
  # runs our config.
  if [ -n "${XOLOTL_DEMO_PID:-}" ] && kill -0 "$XOLOTL_DEMO_PID" 2>/dev/null &&
    tr '\0' ' ' <"/proc/$XOLOTL_DEMO_PID/cmdline" 2>/dev/null | grep -qF -- "$XOLOTL_DEMO_DIR/config.toml"; then
    kill "$XOLOTL_DEMO_PID" 2>/dev/null
    wait "$XOLOTL_DEMO_PID" 2>/dev/null
  fi
  # The container: only the ID `docker run` returned, and only while it
  # carries this run's label.
  if [ -n "${XOLOTL_DEMO_BAO:-}" ] &&
    [ "$(docker inspect -f '{{index .Config.Labels "xolotl-demo"}}' "$XOLOTL_DEMO_BAO" 2>/dev/null)" = "$XOLOTL_DEMO_DIR" ]; then
    docker rm -f "$XOLOTL_DEMO_BAO" >/dev/null 2>&1
  fi
  # The directory: only the one mktemp made for this run.
  case "${XOLOTL_DEMO_DIR:-}" in
    /tmp/xolotl-demo.*) [ -f "$XOLOTL_DEMO_DIR/.xolotl-demo" ] && rm -rf -- "$XOLOTL_DEMO_DIR" ;;
  esac
  unset XOLOTL_DEMO_PID XOLOTL_DEMO_BAO
  trap - EXIT HUP TERM
}

xolotl_demo_free_port() {
  python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

xolotl_demo_up() {
  XOLOTL_DEMO_DIR=$(mktemp -d /tmp/xolotl-demo.XXXXXX) || return 1
  : >"$XOLOTL_DEMO_DIR/.xolotl-demo"
  trap xolotl_demo_down EXIT
  trap 'xolotl_demo_down; exit 129' HUP
  trap 'xolotl_demo_down; exit 143' TERM

  XOLOTL_DEMO_BAO=$(docker run -d --rm --label "xolotl-demo=$XOLOTL_DEMO_DIR" -p 127.0.0.1::8200 \
    openbao/openbao:latest server -dev -dev-root-token-id=demo-root) || return 1
  local bao_port
  bao_port=$(docker port "$XOLOTL_DEMO_BAO" 8200/tcp | head -n1 | sed 's/.*://') || return 1
  local i
  for i in $(seq 60); do
    curl -sf "http://127.0.0.1:$bao_port/v1/sys/health" >/dev/null && break
    sleep 0.5
  done

  bao auth enable approle >/dev/null || return 1
  bao policy write xolotl - >/dev/null <<'HCL' || return 1
path "secret/data/xolotl/*"     { capabilities = ["create", "read", "update", "delete"] }
path "secret/metadata/xolotl/*" { capabilities = ["list", "read", "delete"] }
path "secret/destroy/xolotl/*"  { capabilities = ["update"] }
HCL
  bao write auth/approle/role/xolotl token_policies=xolotl >/dev/null || return 1
  XOLOTL_VAULT_ROLE_ID=$(bao read -field=role_id auth/approle/role/xolotl/role-id) || return 1
  XOLOTL_VAULT_SECRET_ID=$(bao write -f -field=secret_id auth/approle/role/xolotl/secret-id) || return 1
  XOLOTL_ADMIN_TOKEN=$(openssl rand -hex 24) || return 1
  bao kv put secret/xolotl/admin token="$XOLOTL_ADMIN_TOKEN" >/dev/null || return 1
  export XOLOTL_VAULT_ROLE_ID XOLOTL_VAULT_SECRET_ID XOLOTL_ADMIN_TOKEN

  local port
  port=$(xolotl_demo_free_port) || return 1

  # One OpenAI-compatible provider (for example llama.cpp). Its models are
  # static, so the demo needs no running model server.
  cat >"$XOLOTL_DEMO_DIR/config.toml" <<TOML
[server]
listen = "127.0.0.1:$port"

[database]
path = "$XOLOTL_DEMO_DIR/xolotl.db"

[vault]
address = "http://127.0.0.1:$bao_port"

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

  "$PWD/target/debug/xolotl" serve --config "$XOLOTL_DEMO_DIR/config.toml" >"$XOLOTL_DEMO_DIR/server.log" 2>&1 &
  XOLOTL_DEMO_PID=$!
  export XOLOTL_URL="http://127.0.0.1:$port"
  for i in $(seq 60); do
    curl -sf "$XOLOTL_URL/readyz" >/dev/null && break
    sleep 0.5
  done
  if ! curl -sf "$XOLOTL_URL/readyz" >/dev/null; then
    cat "$XOLOTL_DEMO_DIR/server.log"
    return 1
  fi

  export PATH="$PWD/target/debug:$PATH"
}

if ! xolotl_demo_up; then
  echo "demo setup failed; cleaning up" >&2
  xolotl_demo_down
fi
