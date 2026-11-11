---
xolotl-docs: patch
---

The configuration and client docs now say that every endpoint, including the T3 Code hubs endpoints under `/v0/management/`, is served on the one `server.listen` address, and that a reverse proxy must forward that path too.
