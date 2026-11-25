---
xolotl: patch
xolotl-docs: patch
---

Keep the gateway running and retry when NATS is unreachable at startup; invalid local configuration still fails fast. A missing stream or mismatched subjects now logs an error instead of stopping startup. The `xolotl.export.connected` gauge reports connection state, not stream validation; check logs for stream errors. Preserve usable model prices when unrelated registry entries are malformed.
