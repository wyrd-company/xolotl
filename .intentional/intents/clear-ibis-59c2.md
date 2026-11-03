---
xolotl: minor
xolotl-docs: minor
---

Warn when a policy allows a subscription provider with no accounts. Such a provider looks enabled, but its requests get a 403. `xolotl policies put` and `PUT /admin/policies/{name}` still save the policy and now name each such provider, and the web UI's policy editor shows the same warning. `xolotl policies view` and `GET /admin/policies/{name}/view` list the accounts each subscription provider can use, and say `no accounts` where there are none.
