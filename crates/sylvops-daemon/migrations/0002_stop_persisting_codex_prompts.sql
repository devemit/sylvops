DELETE FROM session_prompts;

UPDATE sessions
SET arguments_json = '[]'
WHERE provider_kind = 'codex';
