-- Achievement souvenirs (launcher 4.1.2+).
--
-- The launcher takes a screenshot the moment an achievement pops and files it
-- on the player's profile. One screenshot can cover several achievements that
-- unlocked together, which is why the picture is the row and the achievement
-- names hang off it rather than the other way round.
--
-- The upload is a three-step handshake, so a row exists before its bytes do:
--
--   1. POST /presigned-urls/achievement-image reserves `client_id` and hands
--      back `image_key` + a presigned PUT. Status stays 'pending'.
--   2. PUT /storage/{token} lands the bytes and flips `is_uploaded`.
--   3. PUT /profile/games/achievements arrives with the souvenir alongside the
--      achievements it belongs to, filling in the achievement names and
--      flipping status to 'ready' — the point at which it becomes visible.
--
-- The launcher retries the whole sequence with the same `client_id` until the
-- server acknowledges it, so every step has to be idempotent; `client_id` is
-- what makes that possible.
CREATE TABLE souvenirs (
    -- The id the profile and the launcher refer to a souvenir by.
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The launcher's idempotency key for one capture. Unique per user, so a
    -- retried upload reuses its reservation instead of creating a twin.
    client_id TEXT NOT NULL,
    -- Official game id (`remoteId` on the launcher side), which is what the
    -- achievement sync payload keys on.
    remote_game_id TEXT,
    -- Filled from the sync payload (or the reservation), so the profile can
    -- name the game and the launcher can match souvenirs to its library.
    shop TEXT,
    object_id TEXT,
    -- Storage key of the screenshot, also the `imageKey` the launcher holds.
    image_key TEXT NOT NULL,
    is_uploaded INTEGER NOT NULL DEFAULT 0,
    size_in_bytes INTEGER NOT NULL DEFAULT 0,
    -- 'pending' until the achievement sync claims the reservation, then 'ready'.
    status TEXT NOT NULL DEFAULT 'pending',
    -- Upper-cased achievement the souvenir is filed under: the first name in
    -- the sync payload, which is the achievement that triggered the capture.
    primary_achievement_name TEXT,
    -- JSON array of the upper-cased achievement names captured together.
    achievement_names TEXT NOT NULL DEFAULT '[]',
    -- Launcher clock, epoch milliseconds — the same value the launcher sends
    -- and sorts by, kept verbatim so the two never disagree by a timezone.
    captured_at INTEGER NOT NULL,
    -- Per-souvenir 'PUBLIC' | 'PRIVATE'. The account-level setting on
    -- users.souvenirs_visibility gates the whole tab; this hides one picture.
    visibility TEXT NOT NULL DEFAULT 'PUBLIC',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_souvenirs_client ON souvenirs (user_id, client_id);
CREATE INDEX idx_souvenirs_profile ON souvenirs (user_id, status, captured_at DESC);
CREATE INDEX idx_souvenirs_game ON souvenirs (user_id, shop, object_id);

-- One image belongs to one souvenir: the launcher rotates its client id and
-- re-uploads when a key is already spoken for, so this has to be enforced
-- rather than assumed.
CREATE UNIQUE INDEX idx_souvenirs_image_key ON souvenirs (image_key);

-- Profile likes. Any member of this server may like another's souvenir, so
-- this is keyed by the viewer rather than the owner.
CREATE TABLE souvenir_likes (
    souvenir_id TEXT NOT NULL REFERENCES souvenirs(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (souvenir_id, user_id)
);

CREATE INDEX idx_souvenir_likes_user ON souvenir_likes (user_id);

-- Player reports. Deliberately NOT foreign-keyed to souvenirs: the usual
-- outcome of a report is the picture being deleted, and a moderation record
-- that vanishes with the thing it was about is no record at all. The event log
-- carries the same reports for the History screen; this table is the durable
-- one an operator can query.
CREATE TABLE souvenir_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    souvenir_id TEXT NOT NULL,
    owner_user_id TEXT NOT NULL,
    reporter_user_id TEXT NOT NULL,
    -- 'hate' | 'sexual_content' | 'violence' | 'spam' | 'other'
    reason TEXT NOT NULL,
    description TEXT,
    created_at TEXT NOT NULL
);

-- One report per person per souvenir; a second one is accepted and ignored so
-- the launcher's "reported" state survives a retry.
CREATE UNIQUE INDEX idx_souvenir_reports_unique
    ON souvenir_reports (souvenir_id, reporter_user_id);

CREATE INDEX idx_souvenir_reports_created ON souvenir_reports (created_at DESC);

-- Account-level souvenir privacy, mirrored from the official profile by the
-- launcher (the official API owns the setting; this server only needs to know
-- it to answer for other viewers). 'PRIVATE' by default so nothing is exposed
-- before the launcher has told us what the user actually chose.
ALTER TABLE users ADD COLUMN souvenirs_visibility TEXT NOT NULL DEFAULT 'PRIVATE';
