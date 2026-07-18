-- Per-game custom artwork (covers, heroes, logos, icons) chosen in the
-- launcher. Mirrors Hydra Cloud's "Custom Image Sync": one row per
-- user/game/kind, replaced whenever the user picks a new image.
--
-- `source` is "upload" for a file the user uploaded (stored by this server,
-- with `storage_key` pointing at it) or "steamgriddb" for an image picked
-- from SteamGridDB, where `url` already points at SteamGridDB's CDN and
-- nothing is stored locally.
CREATE TABLE game_artwork (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    url TEXT NOT NULL,
    storage_key TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, shop, object_id, kind)
);

-- Profile views load every artwork a user owns in one query.
CREATE INDEX idx_game_artwork_user ON game_artwork (user_id);
