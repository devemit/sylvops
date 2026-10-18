-- Contract the expand-phase compatibility column after 0009 has preserved legacy
-- seen/unseen completion as acknowledged/pending attention in status_json.
DROP INDEX idx_sessions_attention;
ALTER TABLE sessions DROP COLUMN state;
CREATE INDEX idx_sessions_attention
    ON sessions(json_extract(status_json, '$.attention.acknowledgement'), last_activity_at DESC);
