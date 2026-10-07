-- Who created each work item, for the per-person "work items created" reports.
-- created_by is the display name; created_by_unique the sign-in identity (email-style) that
-- team-roster matching keys on, since display names can differ between a roster and an item.
--
-- Additive (like 0002/0003/0008/0009/0010) so existing DBs migrate in place; filled by the
-- next poll. NULL = the provider didn't say.
ALTER TABLE work_items ADD COLUMN created_by TEXT;
ALTER TABLE work_items ADD COLUMN created_by_unique TEXT;
