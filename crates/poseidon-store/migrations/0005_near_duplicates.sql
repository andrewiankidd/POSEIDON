-- On-demand near-duplicate scan findings. Computed deterministically (TF-IDF cosine
-- over titles) when a person runs the duplicate scan, then surfaced as `near_duplicate`
-- hygiene flags until the next scan. Owner-scoped; replaced wholesale per team scan.
CREATE TABLE IF NOT EXISTS near_duplicate_findings (
    owner        TEXT    NOT NULL,
    team         TEXT    NOT NULL DEFAULT '',
    work_item_id INTEGER NOT NULL,
    detail       TEXT    NOT NULL DEFAULT '',
    created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_near_dup_owner_team ON near_duplicate_findings(owner, team);
CREATE INDEX IF NOT EXISTS idx_near_dup_owner_item ON near_duplicate_findings(owner, work_item_id);
