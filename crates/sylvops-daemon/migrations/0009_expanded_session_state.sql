-- Expand without removing the legacy client representation. JSON contains only bounded
-- enums, one monotonic event identity, its acknowledgement, and a timestamp.
ALTER TABLE sessions ADD COLUMN status_json TEXT NOT NULL
    DEFAULT '{"lifecycle":"starting","activity":"none","attention":null}'
    CHECK (length(status_json) <= 512 AND json_valid(status_json));

UPDATE sessions SET status_json = json_object(
    'lifecycle', CASE
        WHEN state IN ('finished_seen', 'finished_unseen') THEN
            CASE WHEN process_id IS NOT NULL OR process_identity IS NOT NULL
                 THEN 'disconnected' ELSE 'exited' END
        WHEN state IN ('running', 'needs_feedback') THEN 'disconnected'
        ELSE state END,
    'activity', 'none',
    'attention', CASE
        WHEN state IN ('finished_seen', 'finished_unseen', 'failed', 'needs_feedback')
        THEN json_object(
            'id', 1,
            'reason', CASE state
                WHEN 'failed' THEN 'terminal_failure'
                WHEN 'needs_feedback' THEN CASE WHEN failure_reason IS NOT NULL
                    THEN 'recoverable_failure' ELSE 'input_requested' END
                ELSE 'successful_completion' END,
            'acknowledgement', CASE state
                WHEN 'finished_seen' THEN 'acknowledged' ELSE 'pending' END,
            'occurred_at', last_activity_at)
        ELSE NULL END
);

-- A persisted identity is never evidence of ownership after an upgrade/restart.
UPDATE sessions SET state = 'disconnected', process_id = NULL, process_identity = NULL
WHERE state IN ('running', 'needs_feedback')
   OR (state IN ('finished_seen', 'finished_unseen')
       AND (process_id IS NOT NULL OR process_identity IS NOT NULL));
UPDATE sessions SET process_id = NULL, process_identity = NULL;
