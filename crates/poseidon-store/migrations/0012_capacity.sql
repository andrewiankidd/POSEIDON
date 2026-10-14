-- Capacity reporting. Two small local stores, both advisory and rebuilt from the tracker or the AI:
--
-- commit_activity: who committed to which repository and when - only the timestamps, never the
-- content. It feeds the working-hours picture (evenings, weekends, long streaks). Collected on a
-- slow cadence and pruned to a rolling window.
--
-- demand_class: an AI model's verdict on what KIND of demand an untagged work item is (reactive,
-- planned, maintenance, standing). Kept locally and never written back to the tracker; the title
-- hash lets a retitled item be judged again.
--
-- Additive (like 0002/0003/0008-0011) so existing DBs migrate in place.
CREATE TABLE IF NOT EXISTS commit_activity (
    owner        TEXT NOT NULL,
    team         TEXT NOT NULL,
    repo         TEXT NOT NULL,
    id           TEXT NOT NULL,
    author       TEXT NOT NULL,
    author_email TEXT,
    authored_at  TEXT NOT NULL,
    PRIMARY KEY (owner, team, repo, id)
);
CREATE INDEX IF NOT EXISTS idx_commit_activity_when ON commit_activity (owner, authored_at);

CREATE TABLE IF NOT EXISTS demand_class (
    owner         TEXT NOT NULL,
    team          TEXT NOT NULL,
    work_item_id  INTEGER NOT NULL,
    class         TEXT NOT NULL,
    source        TEXT,
    title_hash    TEXT NOT NULL,
    classified_at TEXT NOT NULL,
    PRIMARY KEY (owner, team, work_item_id)
);
