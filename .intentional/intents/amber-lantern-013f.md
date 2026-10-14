---
xolotl: minor
---

Sign in to the web UI with OpenID Connect (tested with Pocket ID) instead of a password. Configure `[ui.oidc]` (issuer, client ID, redirect URL, admin group) and store the client secret in the vault secret `oidc`; only members of the admin group get a session. `xolotl ui set-password`, `PUT /admin/ui/password` and `POST /admin/session` are removed, and the `ui_password_hash` field of the `admin` secret is no longer read.
