-- Users are mirrored from the official Hydra API: the launcher sends its
-- official access token and this server validates it upstream, so there are
-- no local passwords. `id` is the official Hydra user id.
CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT,
    display_name TEXT NOT NULL DEFAULT '',
    profile_image_url TEXT,
    is_blocked INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);

-- Cloud save backups (tar bundles produced by Ludusavi on the client).
CREATE TABLE artifacts (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    artifact_length_in_bytes INTEGER NOT NULL,
    hostname TEXT NOT NULL DEFAULT '',
    wine_prefix_path TEXT,
    home_dir TEXT NOT NULL DEFAULT '',
    download_option_title TEXT,
    platform TEXT,
    label TEXT,
    is_frozen INTEGER NOT NULL DEFAULT 0,
    is_uploaded INTEGER NOT NULL DEFAULT 0,
    download_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_artifacts_user_game ON artifacts(user_id, shop, object_id);

-- Achievement sync. Keyed by the OFFICIAL server's game id (remoteId) since
-- that's what the launcher sends; shop/object_id come from the augmented
-- request body. Achievements are stored as the raw JSON array the client
-- sent, merged by achievement name.
CREATE TABLE game_achievements (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    remote_game_id TEXT NOT NULL,
    shop TEXT,
    object_id TEXT,
    achievements TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, remote_game_id)
);

-- Download source URLs synced across the user's devices.
CREATE TABLE download_sources (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    name TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (user_id, url)
);

-- Emulator memory-card saves (PS1 .mcs / PS2 .psu files).
CREATE TABLE emulation_saves (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    platform TEXT NOT NULL,
    emulator TEXT NOT NULL,
    save_kind TEXT NOT NULL DEFAULT 'game_save',
    save_identity TEXT NOT NULL,
    artifact_length_in_bytes INTEGER NOT NULL DEFAULT 0,
    file_name TEXT,
    hostname TEXT,
    local_last_modified_at TEXT,
    label TEXT,
    metadata TEXT,
    shop TEXT,
    object_id TEXT,
    is_uploaded INTEGER NOT NULL DEFAULT 0,
    last_uploaded_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_emulation_saves_user ON emulation_saves(user_id, platform, emulator);
