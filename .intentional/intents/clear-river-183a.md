---
xolotl: minor
xolotl-docs: minor
---

NATS export: with `[export.nats]`, xolotl publishes each exported exchange to a NATS JetStream stream, and ClickHouse loads the stream with its NATS table engine. Rows published while ClickHouse is down wait in the stream; rows waiting while NATS restarts are held in xolotl's queue and retried until NATS takes them. A row too large for a message has its bodies cut, or is refused and counted. Xolotl checks at startup that the stream exists and takes `xolotl.exchanges.>`. The new NATS export page gives the stream, consumer and table commands, permissions, sizing and alerts. The direct ClickHouse export is unchanged. Building from source needs Rust 1.89.
