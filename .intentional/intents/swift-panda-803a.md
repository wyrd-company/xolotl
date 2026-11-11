---
xolotl: minor
xolotl-docs: minor
---

The ClickHouse and NATS exports now cover every kind of account, not only the sessions of subscription accounts: provider accounts (Vercel, Mistral, OpenAI-compatible) and API key accounts are exported too, and so are requests without a session, such as embeddings, OCR and passthroughs. A new `account_kind` column names the kind of the account that served each row. Responses that are not text, such as audio, and passthrough uploads that are not JSON are exported without their bodies. A table made earlier keeps receiving rows without the new column; the ClickHouse and NATS export pages give the `ALTER TABLE` that adds it.
