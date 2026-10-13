---
xolotl: minor
---

Add an admin web UI at `/`: accounts and logins, policies with a live preview of the names a key sees (including translated endpoints), client keys, provider keys and the model catalog, and usage. Sign in with one password, set with `xolotl ui set-password` and stored as an argon2id hash in OpenBao. New admin endpoints `GET /admin/status`, `GET /admin/providers`, `GET /admin/catalog/routes` and `POST /admin/policy-preview`, and `telemetry.dashboard_url` for the link to OpenObserve.
