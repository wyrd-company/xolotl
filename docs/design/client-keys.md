# Client keys

Clients authenticate with keys issued by xolotl (`xol_…`). `src/keys.rs` manages them together with the policies they belong to.

## Storage

| OpenBao path (under the prefix) | Contents |
|---|---|
| `policies/<policy>` | policy definition |
| `keys/<policy>/<client>` | key record: `api_key`, `client_id`, `policy`, `name`, timestamps, `expires_at`, `disabled`, previous key hash and expiry during a rotation grace period |

The record holds the plaintext key so the client can fetch it itself, with a Vault policy scoped to that one path. The CLI never prints keys, so no operator has to see them. This follows the `client-key-policy` design in `.references/cpa-plugin-registry`.

Xolotl itself keeps only SHA-256 hashes in memory.

## Authentication

`KeyManager::authenticate` hashes the presented key and looks it up. The match succeeds if:

- the key's current hash matches, or the previous hash matches within its grace period;
- the key is not expired, disabled or revoked;
- its policy exists.

The result is a principal: client id plus policy.

- **Reloads:** keys and policies reload every 60 s. An unknown `xol_` key also triggers a reload, at most once per 10 s, so a key issued elsewhere works without waiting for the next periodic reload.
- **Bootstrap admin token:** the token stored at `admin` authenticates with an unrestricted policy and the `admin` scope. The CLI uses it.

## Lifecycle

All writes use KV v2 check-and-set against the version that was read, so an instance with stale data gets a conflict instead of overwriting a newer record.

| Operation | Effect |
|---|---|
| issue | New random key under `<policy>/<name>`. Refused if a live key with that name exists (rotate it instead); a revoked name can be issued again. |
| rotate `--grace` | New key; the old key's hash and a grace expiry are kept in the record so both work until then |
| disable / enable | Flag in the record |
| revoke | Writes a revocation marker (no key) as the newest version, then **destroys** the data of every earlier version, so the key can't be recovered from history. Requires `update` on `secret/destroy/xolotl/*`. |

A policy can't be deleted while keys still reference it. Policy writes are validated: known providers, winners and alias targets allowed by the policy, and valid names.
