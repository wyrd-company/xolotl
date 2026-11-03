---
xolotl-docs: patch
---

Say what happens to a code mode tool call that the code does not await: the gateway stops waiting for it when the code returns, and it may never start or may still run on the upstream.
