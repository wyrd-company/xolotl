-- relationships:
--   implements: ../docs/design/model-metadata.md
CREATE TABLE model_metadata_cache (id INTEGER PRIMARY KEY CHECK (id = 1), body TEXT NOT NULL);
ALTER TABLE request_log ADD COLUMN api_equivalent_cost REAL;
