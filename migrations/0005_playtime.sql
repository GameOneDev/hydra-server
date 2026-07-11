-- Daily playtime buckets reported by the launcher and read back for the
-- profile playtime heatmap. `day` is the player's local calendar date
-- (YYYY-MM-DD) as reported by the client, so the chart matches their
-- timezone rather than the server's.
CREATE TABLE playtime_daily (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    seconds INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, day, shop, object_id)
);

CREATE INDEX idx_playtime_daily_user_day ON playtime_daily(user_id, day);
