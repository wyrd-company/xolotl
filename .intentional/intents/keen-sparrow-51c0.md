---
xolotl: major
xolotl-docs: minor
---

Track each conversation and keep long ones on the account that holds their prompt cache. Xolotl now identifies the session of every LLM request and records its size against the model's context window, its peak, its turns and its cache hit ratio. They are in the request log, in new metrics, at `GET /admin/sessions` and on the Usage screen. The tools each key calls are at `GET /admin/tools` and on the same screen.

A session whose prompt reaches `sessions.lock_tokens` (default 60 000) now stays on its account and model, past the soft limit and ahead of alias order. The lock breaks on a cooldown, a lost account, a policy or limit change, or `subscription.max_attempts` failed requests in a row.

Upgrade note: a locked session's client now sees a 5xx or a transport error from its account, up to that limit, where it used to fail over. Set `lock_tokens = 0` to keep the old failover.
