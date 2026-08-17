-- On-demand AI healthcheck findings. Like ai_tag_suggestions, these are computed
-- by the active AI backend (server online or the browser's WebGPU model) when a
-- person runs the healthcheck, then surfaced as `ai_audit` hygiene flags until the
-- next run. Owner-scoped; replaced wholesale per item on each run.
CREATE TABLE IF NOT EXISTS ai_audit_findings (
    owner        TEXT    NOT NULL,
    team         TEXT    NOT NULL DEFAULT '',
    work_item_id INTEGER NOT NULL,
    kind         TEXT    NOT NULL,
    detail       TEXT    NOT NULL DEFAULT '',
    created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_ai_audit_owner_team ON ai_audit_findings(owner, team);
CREATE INDEX IF NOT EXISTS idx_ai_audit_owner_item ON ai_audit_findings(owner, work_item_id);
