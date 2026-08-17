-- POSEIDON initial schema.
--
-- Every table carries an `owner` column, defaulted to 'default' in the PoC
-- (the hosted instance is gated at the mesh, no app-level auth yet). When
-- multi-user auth lands, real user ids replace 'default' and existing rows
-- migrate under it - no reshape needed. This is the one forward-compat
-- decision baked in from day one.
--
-- `team` is POSEIDON's grouping unit: a named board - an Azure DevOps project,
-- optionally narrowed to an area path. It holds the `[[team]]` config `name`.
--
-- Timestamps are stored as RFC3339 TEXT so they sort lexically (ISO-8601) -
-- date-range report queries are plain string comparisons, and the format is
-- portable across the SQLite file moving between desktop, container PV, and
-- portable USB stick.
--
-- Pre-1.0 convention: this migration is edited in place as the schema evolves
-- and the DB is wiped + re-provisioned. Additive migrations only start once 1.0
-- tagged binaries ship (real data in the wild).

CREATE TABLE IF NOT EXISTS work_items (
    owner          TEXT    NOT NULL,
    provider       TEXT    NOT NULL,
    team           TEXT    NOT NULL,
    id             INTEGER NOT NULL,
    title          TEXT    NOT NULL DEFAULT '',
    work_item_type TEXT    NOT NULL DEFAULT '',
    state          TEXT    NOT NULL DEFAULT '',
    tags           TEXT    NOT NULL DEFAULT '[]',  -- JSON array of strings
    assigned_to    TEXT,
    created_at     TEXT    NOT NULL,
    changed_at     TEXT    NOT NULL,
    closed_at      TEXT,
    iteration_path TEXT,
    story_points   REAL,
    url            TEXT    NOT NULL DEFAULT '',
    linked_pr_ids  TEXT    NOT NULL DEFAULT '[]',  -- JSON array of PR ids
    description    TEXT,                            -- body/description (HTML stripped), for tag suggestion
    PRIMARY KEY (owner, provider, team, id)
);

CREATE INDEX IF NOT EXISTS idx_work_items_owner_team ON work_items(owner, team);
CREATE INDEX IF NOT EXISTS idx_work_items_closed_at  ON work_items(owner, closed_at);
CREATE INDEX IF NOT EXISTS idx_work_items_created_at ON work_items(owner, created_at);

CREATE TABLE IF NOT EXISTS pipelines (
    owner    TEXT    NOT NULL,
    provider TEXT    NOT NULL,
    team     TEXT    NOT NULL,
    id       INTEGER NOT NULL,
    name     TEXT    NOT NULL DEFAULT '',
    folder   TEXT,
    url      TEXT    NOT NULL DEFAULT '',
    last_run_status TEXT,
    last_run_at     TEXT,
    last_run_url    TEXT,
    PRIMARY KEY (owner, provider, team, id)
);

CREATE TABLE IF NOT EXISTS pipeline_runs (
    owner         TEXT    NOT NULL,
    provider      TEXT    NOT NULL,
    team          TEXT    NOT NULL,
    id            INTEGER NOT NULL,
    pipeline_id   INTEGER NOT NULL,
    status        TEXT    NOT NULL,           -- RunStatus serialised (snake_case)
    started_at    TEXT,
    finished_at   TEXT,
    source_branch TEXT,
    url           TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (owner, provider, team, id)
);

CREATE INDEX IF NOT EXISTS idx_runs_pipeline ON pipeline_runs(owner, pipeline_id);
CREATE INDEX IF NOT EXISTS idx_runs_finished ON pipeline_runs(owner, finished_at);

CREATE TABLE IF NOT EXISTS pull_requests (
    owner          TEXT    NOT NULL,
    provider       TEXT    NOT NULL,
    team           TEXT    NOT NULL,
    id             INTEGER NOT NULL,
    title          TEXT    NOT NULL DEFAULT '',
    status         TEXT    NOT NULL DEFAULT 'active', -- PrStatus slug (snake_case)
    is_draft       INTEGER NOT NULL DEFAULT 0,        -- 0/1 bool
    repository     TEXT,
    author         TEXT,
    created_at     TEXT,
    source_branch  TEXT,
    target_branch  TEXT,
    reviewer_count INTEGER NOT NULL DEFAULT 0,
    url            TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (owner, provider, team, id)
);

CREATE INDEX IF NOT EXISTS idx_prs_team ON pull_requests(owner, team);

-- Small key/value scratch for poll metadata (e.g. last_polled_at).
CREATE TABLE IF NOT EXISTS meta (
    owner TEXT NOT NULL,
    key   TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (owner, key)
);

-- Per-owner configuration (teams, rules, doctor state, poll scope) as a JSON
-- blob keyed by owner. This is the multi-tenant seam: standalone has one owner
-- ("default"); a hosted deployment maps each authenticated user to their own
-- row. Instance-level settings ([server], telemetry) stay in TOML/env.
CREATE TABLE IF NOT EXISTS user_config (
    owner       TEXT NOT NULL PRIMARY KEY,
    config_json TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT ''
);

-- User-saved report definitions (a serialised ReportSpec per row). Built-in
-- templates live in code, not here. Owner-scoped for the multi-user roadmap.
CREATE TABLE IF NOT EXISTS reports (
    owner      TEXT NOT NULL,
    name       TEXT NOT NULL,
    spec_json  TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (owner, name)
);

-- AI-suggested tags per work item, precomputed by a run of the AI tagger (a
-- model call per item is too slow to do on read). Advisory only - surfaced as
-- the same suggestion chips the keyword suggester feeds; a person applies them.
-- Owner-scoped; `team` lets a re-run clear a team's suggestions first.
CREATE TABLE IF NOT EXISTS ai_tag_suggestions (
    owner        TEXT    NOT NULL,
    team         TEXT    NOT NULL DEFAULT '',
    work_item_id INTEGER NOT NULL,
    tag          TEXT    NOT NULL,
    reason       TEXT    NOT NULL DEFAULT '',
    created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (owner, work_item_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_ai_sugg_owner_team ON ai_tag_suggestions(owner, team);
