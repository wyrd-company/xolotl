---
xolotl: patch
xolotl-docs: patch
---

Keep policies from before 0.4.0 working after an upgrade. Upgrading to 0.4.0 or 0.5.0 disabled every stored policy with `allow_models` or `deny_models`, and its keys got `401 invalid API key`. Such a policy now migrates to `rules` and `accounts` with the same effect when it loads. A stored policy that does not load now shows in `xolotl policies list`, `policies get`, `GET /admin/status` and the web UI, and a valid key refused because of it is logged with its client and policy.

Upgrade note: a policy from before 0.4.0 with neither `allow_models` nor `deny_models` can't be told apart from a current policy that uses no accounts, so it is not migrated. Its keys get a 403 from each subscription provider. To restore access, add `accounts` to the policy, for example `"accounts": {"claude": ["*"]}`, and save it with `xolotl policies put`.
