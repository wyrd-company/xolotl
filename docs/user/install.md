---
title: Installation
order: 2
install: true
product: xolotl
---

Xolotl is one executable, `xolotl`. `xolotl serve` runs the server. Every other
command is a client of a running server.

You need:

- an [OpenBao](https://openbao.org) (or Vault) server with a KV v2 mount;
- a place to keep a SQLite file (runtime state only, no secrets).

## Container image

The image is `ghcr.io/wyrd-company/xolotl` for `linux/amd64` and `linux/arm64`.
Tags are `X.Y.Z`, `X.Y` and `latest`.

The image runs `xolotl serve --config /etc/xolotl/config.toml` as a non-root
user, keeps its database in the `/data` volume, and listens on port 8317.

```sh
docker run -d --name xolotl -p 8317:8317 \
  -v "$PWD/config.toml:/etc/xolotl/config.toml:ro" \
  -v xolotl-data:/data \
  -e XOLOTL_VAULT_ROLE_ID=... -e XOLOTL_VAULT_SECRET_ID=... \
  ghcr.io/wyrd-company/xolotl:latest
```

## Release binary

Each [GitHub release](https://github.com/wyrd-company/xolotl/releases) has a
Linux x86_64 archive with a `.sha256` file. The web UI is built into the binary.

```sh
version=0.2.0
curl -LO "https://github.com/wyrd-company/xolotl/releases/download/$version/xolotl-$version-x86_64-unknown-linux-gnu.tar.gz"
tar xzf "xolotl-$version-x86_64-unknown-linux-gnu.tar.gz"
"xolotl-$version-x86_64-unknown-linux-gnu/xolotl" --version
```

You can also use this binary as the CLI on your workstation.

## Build from source

You need Rust 1.88 or later, and Node 22 with pnpm for the web UI.

```sh
git clone https://github.com/wyrd-company/xolotl.git && cd xolotl
(cd ui && pnpm install && pnpm build)   # the release build embeds ui/dist
cargo build --release --locked
./target/release/xolotl --version
```

## Prepare OpenBao

Xolotl keeps every secret in a KV v2 mount under one prefix (default
`secret/xolotl/`). It logs in with AppRole. Give the role read and write access
to the prefix, and `update` on `destroy` so that revoked client keys can be
destroyed:

```sh
bao auth enable approle
bao policy write xolotl - <<'HCL'
path "secret/data/xolotl/*"     { capabilities = ["create", "read", "update", "delete"] }
path "secret/metadata/xolotl/*" { capabilities = ["list", "read", "delete"] }
path "secret/destroy/xolotl/*"  { capabilities = ["update"] }
HCL
bao write auth/approle/role/xolotl token_policies=xolotl token_ttl=1h token_max_ttl=24h
bao read -field=role_id auth/approle/role/xolotl/role-id
bao write -f -field=secret_id auth/approle/role/xolotl/secret-id
```

Give the role ID and secret ID to xolotl in `XOLOTL_VAULT_ROLE_ID` and
`XOLOTL_VAULT_SECRET_ID`, or in files (see [Configuration](configuration)).

Then store the bootstrap admin token. The CLI uses it:

```sh
bao kv put secret/xolotl/admin token="$(openssl rand -base64 32)"
```

The [Configuration](configuration#openbao-paths) page lists every path xolotl
reads and writes.

## Run

Copy `config.example.toml` from the repository (it is also in the release
archive), set at least `[vault]`, then check it:

```sh
xolotl check-config --config config.toml   # prints the config with credentials redacted
xolotl serve --config config.toml
```

The server reads policies, client keys, accounts and MCP servers from OpenBao
when it starts. It does not start if it can't reach OpenBao.

| Endpoint | Use |
|---|---|
| `GET /healthz` | Liveness. Always 200 while the process runs. |
| `GET /readyz` | Readiness. Checks SQLite and OpenBao. |

## Point the CLI at the server

```sh
export XOLOTL_URL=http://xolotl.example.lan:8317     # default http://127.0.0.1:8317
export XOLOTL_ADMIN_TOKEN=$(bao kv get -field=token secret/xolotl/admin)
xolotl accounts list
```

Next: [add accounts](accounts), or set up the [web UI](web-ui).
