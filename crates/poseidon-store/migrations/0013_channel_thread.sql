-- Chat-channel threads (Microsoft Teams): who started each conversation, who answered and when.
-- Metadata only - the message text is never stored. Feeds the Interruptions picture on the
-- Capacity screen. Rebuilt from the channel on a slow cadence and pruned to a rolling window.
--
-- Additive (like 0012) so existing DBs migrate in place.
CREATE TABLE IF NOT EXISTS channel_thread (
    owner            TEXT NOT NULL,
    team             TEXT NOT NULL,
    channel_key      TEXT NOT NULL,
    id               TEXT NOT NULL,
    channel_name     TEXT NOT NULL,
    author           TEXT NOT NULL,
    automated        INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    urgent           INTEGER NOT NULL DEFAULT 0,
    web_url          TEXT,
    replies_json     TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (owner, team, channel_key, id)
);
CREATE INDEX IF NOT EXISTS idx_channel_thread_when ON channel_thread (owner, created_at);
