-- Parent hierarchy id + linked repo names on a work item, for tag inheritance
-- (a child inherits its parent's product/area) and repo->tag rules (a PR to
-- "PlatformDeployment" implies area:platform-deployment). Both come free with the
-- poll (from the item's relations), so they live on work_items and are set on each
-- upsert - additive columns, default NULL. Additive (not folded into 0001) to keep
-- existing live DBs migrating cleanly rather than needing a wipe.
ALTER TABLE work_items ADD COLUMN parent_id INTEGER;
ALTER TABLE work_items ADD COLUMN linked_repos TEXT;
