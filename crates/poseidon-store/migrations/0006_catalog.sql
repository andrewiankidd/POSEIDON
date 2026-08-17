-- Service catalog synced from an internal developer portal (Port / Backstage) or a
-- CSV export: repo -> product / team / domain, so `product:*` tags derive from the
-- catalog instead of a hand-maintained repo_tags map. Owner-scoped; a sync replaces
-- the whole snapshot (like a poll rewrites work_items). repo is the join key to
-- WorkItem.linked_repos, so only repo-bearing rows are stored.
-- See docs/design/catalog-integration.md.
--
-- Additive (not folded into 0001) on purpose: live instances already carry polled
-- data, so editing 0001 would trip sqlx's checksum guard and force a DB wipe.
CREATE TABLE IF NOT EXISTS catalog (
    owner    TEXT NOT NULL,
    repo     TEXT NOT NULL,
    product  TEXT,
    team     TEXT,
    domain   TEXT,
    kind     TEXT,
    PRIMARY KEY (owner, repo)
);
CREATE INDEX IF NOT EXISTS idx_catalog_owner ON catalog(owner);
