-- Tracker board columns. A Kanban board's columns are not work-item states (a team can
-- add "To prioritize" next to "New", both mapping to State=New), so each work item now
-- records the column the tracker's board shows it in, plus its manual board order, and
-- each team's board definitions are kept so the UI can mirror the tracker's own board.
--
-- Additive (like 0002/0003) so existing DBs migrate in place; the columns are filled by
-- the next poll. NULL = the provider has no board columns.
ALTER TABLE work_items ADD COLUMN board_column TEXT;
ALTER TABLE work_items ADD COLUMN board_column_done INTEGER;
ALTER TABLE work_items ADD COLUMN board_lane TEXT;
ALTER TABLE work_items ADD COLUMN backlog_rank REAL;

-- One row per (owner, team): the team's boards as JSON (`Vec<Board>`), replaced on every
-- successful poll that discovers boards.
CREATE TABLE IF NOT EXISTS team_boards (
    owner  TEXT NOT NULL,
    team   TEXT NOT NULL,
    boards TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (owner, team)
);
