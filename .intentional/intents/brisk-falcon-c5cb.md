---
xolotl: patch
xolotl-docs: patch
---

Codex shows its usage limits when the Codex pool runs out. When every Codex account is cooling down, the 429 now carries the `x-codex-*` usage headers of the account that comes back first. A quota error at the start of a stream keeps the account's usage headers and uses the error shape Codex reads. Before, Codex showed no limits at that point. The Codex client guide explains where Codex gets its limits through xolotl, and why `/status` has none before the first turn.
