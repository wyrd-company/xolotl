-- When the request was admitted; `ts` is when it finished. Rate limits
-- count requests at admission and tokens at completion.
ALTER TABLE request_log ADD COLUMN started_at INTEGER;
