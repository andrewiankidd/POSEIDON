-- Durable AI state, so a browser refresh no longer loses work.
--
-- 1. ai_field_drafts: the "Improve all fields" per-field drafts, persisted server-side
--    (previously client-only in localStorage). Cleared per item when reviewed/applied.
--    This is what makes the ✨ badge + editor pre-fill survive a refresh / another
--    machine, and unifies improve-all with tag-suggestions + audit (already DB-backed).
--
-- 2. ai_activity: an append/upsert log of AI jobs (one row per run) with their per-item
--    results, so the activity queue can be VIEWED post-refresh and doubles as an audit
--    trail (what the AI proposed, when, for whom). Owner-scoped like everything else.

CREATE TABLE IF NOT EXISTS ai_field_drafts (
    owner        TEXT    NOT NULL,
    team         TEXT    NOT NULL DEFAULT '',
    work_item_id INTEGER NOT NULL,
    field_ref    TEXT    NOT NULL,
    value        TEXT    NOT NULL DEFAULT '',
    created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (owner, work_item_id, field_ref)
);
CREATE INDEX IF NOT EXISTS idx_ai_drafts_owner_item ON ai_field_drafts(owner, work_item_id);

CREATE TABLE IF NOT EXISTS ai_activity (
    id         TEXT    NOT NULL,                       -- stable client job id
    owner      TEXT    NOT NULL,
    team       TEXT    NOT NULL DEFAULT '',
    name       TEXT    NOT NULL,                       -- e.g. 'Suggest tags'
    where_at   TEXT    NOT NULL DEFAULT '',            -- 'gpu' | 'server'
    status     TEXT    NOT NULL,                       -- running | done | failed | cancelled
    done       INTEGER NOT NULL DEFAULT 0,
    total      INTEGER NOT NULL DEFAULT 0,
    outcome    TEXT    NOT NULL DEFAULT '',
    items_json TEXT    NOT NULL DEFAULT '[]',          -- per-item results (JSON array)
    started_at TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (owner, id)
);
CREATE INDEX IF NOT EXISTS idx_ai_activity_owner ON ai_activity(owner, updated_at DESC);
