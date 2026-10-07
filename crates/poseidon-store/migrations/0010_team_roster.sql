-- Team rosters, so per-person reports cover only the people who actually belong to a team.
-- A project-wide pull-request feed includes one-off contributors from other teams; the
-- tracker's own team membership says who is on this one.
--
-- Additive (like 0002/0003/0008/0009) so existing DBs migrate in place; filled by the
-- next poll. NULL = the provider didn't give a sign-in identity.
ALTER TABLE pull_requests ADD COLUMN author_unique TEXT;

-- One row per (owner, team): the team's roster as JSON (`Vec<TeamMember>`), replaced on
-- every poll that reads a non-empty roster.
CREATE TABLE IF NOT EXISTS team_members (
    owner   TEXT NOT NULL,
    team    TEXT NOT NULL,
    members TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (owner, team)
);
