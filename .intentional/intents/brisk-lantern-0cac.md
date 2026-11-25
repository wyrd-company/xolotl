---
xolotl: minor
xolotl-docs: minor
---

Add an opt-in authenticated HTTPS forward proxy for Antigravity model traffic, with allowlisted interception, OpenBao-issued certificates and exchange export to NATS and ClickHouse. Other destinations remain encrypted tunnels. Configure the listener, trusted CA and restricted PKI role before enabling it; see the forward proxy documentation. OpenTelemetry reports connections, intercepted requests and certificate issuance through `xolotl.forward_proxy.connections`, `xolotl.forward_proxy.requests` and `xolotl.forward_proxy.certificates`.
