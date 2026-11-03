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

- **Refusals:** a key whose policy is not loaded fails closed. The client gets the same 401 as for an unknown key. The log names the client and the policy at warn level.
- **Reloads:** keys and policies reload every 60 s. An unknown `xol_` key also triggers a reload, at most once per 10 s, so a key issued elsewhere works without waiting for the next periodic reload.
- **Bootstrap admin token:** the token stored at `admin` authenticates with an unrestricted policy and the `admin` scope. The CLI uses it.

## Lifecycle

All writes use KV v2 check-and-set against the version that was read, so an instance with stale data gets a conflict instead of overwriting a newer record.

| Operation | Effect |
|---|---|
| issue | New random key under `<policy>/<name>`; a revoked name can be issued again. If a live key has that name, including a disabled or expired one, it answers `exists` with the stored record and writes nothing. A record that can't be read is an error, never overwritten. |
| rotate `--grace` | New key; the old key's hash and a grace expiry are kept in the record so both work until then |
| disable / enable | Flag in the record |
| revoke | Writes a revocation marker (no key) as the newest version, then **destroys** the data of every earlier version, so the key can't be recovered from history. Requires `update` on `secret/destroy/xolotl/*`. A key already revoked or absent answers without an error, and the destroy runs again. |

Issue and revoke are safe to repeat, so a caller retries after any failure:

- **A lost answer:** the write happened, so the retry reads the key (`exists`) or the marker (`already_revoked`), and the cache catches up.
- **A failed destroy:** the marker is written and the key stops at once, but the answer is an error, because a version may still hold the key. The retry runs the destroy again.
- **A lost race:** another instance wrote first, so check-and-set fails. The operation reads the record again once. A second conflict answers 409.
- **Deleted in OpenBao:** the latest version has no data but keeps its number. Revoke answers `absent`, destroys every version, and removes the key from the cache.

## Keys that manage keys

A policy with the `keys` scope lists policies in `key_policies`. Its keys can issue, revoke, disable and list keys under those policies, and reach no other route. `admin` implies it for every policy. A web UI session counts as `admin`.

`KeyManager::may_manage` decides, against the caller's policy and the listed policy as loaded now. It decides under `writes`, which policy changes also hold. So on one instance, a key change that waited behind a policy change sees that change. Another instance sees it at its next reload, within 60 s, and until then it decides from the policies it has loaded:

- **Only client use:** a listed policy must grant nothing beyond `proxy` and `mcp`. `admin` and `management` reach accounts or more. `keys` could list a policy the issuer does not, which would widen its reach. The check is on the target's scopes, and not at save time, because the target can change after it is listed. A scope added later is refused until it is judged here.
- **Not loaded:** a listed policy that is stored but did not load can't be checked, so it is refused; an operator with `admin` cleans it up. A listed policy that does not exist passes, which lets a retried revoke finish. Issue under it still fails: no such policy.
- **No enable, no rotate:** the issuer must not be able to undo an operator's disable. It can revoke and issue again, which ends in a new key the operator can see in the log.

Revoke, disable, enable and rotate take names from the URL. A decoded name can hold `/`, `..` or `?`, and the vault client puts names into the URL as they are, so the routes refuse a name outside the format.

## Audit

The log is the audit record. Each change to a key logs `actor`: the client ID, or `admin` for the bootstrap token and web UI sessions. A refusal under `key_policies` logs at warn level with `actor`, `policy` and `action`. Xolotl has no separate audit store.

## Policies

A stored policy that can't be read or parsed is left out of the loaded state and listed in `unloaded_policies` of `GET /admin/status`, with its error. Saving or deleting that name clears the entry.

A record with `allow_models` or `deny_models` was written before 0.4.0. `policy::from_stored` migrates it in memory with the same effect, at every reload. A reload does not write it back: `delete_policy` removes every version, so a recreated policy can have the version number a reload read, and check-and-set can't stop a stale migration from overwriting it. Saving the policy stores the current form. A record without those fields is not migrated: no `accounts` is also how a current policy says it has none, so granting accounts there would widen access.

A policy can't be deleted while keys still reference it. Policy writes are validated: known providers, winners and alias targets allowed by the policy, valid names, `accounts` entries of allowed subscription providers that list that provider's account IDs (or `*`; no entry means no accounts), `models` keys of the form `provider/model`, and provider and account limits on what the policy uses.
