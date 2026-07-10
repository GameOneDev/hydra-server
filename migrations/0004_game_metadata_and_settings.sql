-- Cached game names/covers so the admin panel can show real games instead
-- of raw shop ids. Resolved lazily from public shop metadata (Steam store
-- API); `name` stays NULL when a lookup fails so it can be retried later.
CREATE TABLE game_metadata (
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    name TEXT,
    cover_url TEXT,
    fetched_at TEXT NOT NULL,
    PRIMARY KEY (shop, object_id)
);

-- Settings edited from the admin panel. Rows here override the matching
-- environment variables until deleted (reset), so quota changes survive
-- restarts without touching the deployment.
CREATE TABLE server_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
