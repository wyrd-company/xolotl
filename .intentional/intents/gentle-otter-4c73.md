---
xolotl: major
xolotl-docs: minor
---

Let a tool issue client keys without the admin token. A policy with the new `keys` scope lists its `key_policies`; its keys can issue, revoke, disable and list keys under those policies, and reach nothing else. Each change to a key logs the key that acted.

Issuing and revoking are now safe to repeat. `POST /admin/keys` on a name with a live key answers 200 with `status: exists` and no key, where it was a 400, and a new key answers 201 where it was 200, with `status: created`. `DELETE /admin/keys/<policy>/<name>` on a key that is missing or already revoked answers 200, where it was a 404, and its `status` says what it found. Vault failures on both answer 500, where issuing gave a 400 and revoking a 404.

Upgrade note: a script that treats the 400 on a repeated issue, or the 404 on a repeated revoke, as a signal must read `status` instead.
