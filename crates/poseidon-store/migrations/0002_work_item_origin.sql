-- Immutable "where a work item was CREATED" (first-revision area path), fetched
-- lazily from the tracker's revision history. Kept in its OWN table because the
-- work_items table is wiped + reinserted every poll, and this never changes - so
-- it's fetched once per item and reused. Powers the "moved in from another board"
-- source signal (e.g. work created on the SRE board then moved to this team).
--
-- Additive (not folded into 0001) on purpose: this shipped after real polled data
-- existed on live instances, so editing 0001 would trip sqlx's checksum guard and
-- force a DB wipe. Adding it as a new migration lets an existing DB pick it up.
CREATE TABLE IF NOT EXISTS work_item_origin (
    owner        TEXT    NOT NULL,
    work_item_id INTEGER NOT NULL,
    origin_area  TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (owner, work_item_id)
);
