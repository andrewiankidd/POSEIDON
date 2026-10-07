-- Per-contributor pull-request reporting needs two facts the PR screen never did: when a PR
-- was closed (merged / abandoned), so "merged in the last N days" and time-to-merge are
-- answerable, and who reviewed it (name + vote) so reviews given can be counted.
--
-- Additive (like 0002/0003/0008) so existing DBs migrate in place; both are filled by the
-- next poll. NULL / '[]' = the provider didn't say.
ALTER TABLE pull_requests ADD COLUMN closed_at TEXT;
ALTER TABLE pull_requests ADD COLUMN reviewers TEXT NOT NULL DEFAULT '[]';
