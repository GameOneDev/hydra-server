-- Achievement souvenirs (upstream hydralauncher/hydra#2700).
--
-- A screenshot taken when an achievement pops, filed on the player's profile.
-- One picture can cover several achievements that unlocked together, which is
-- why the picture is the row and the names hang off it.
--
-- Uploading takes three calls, so a row exists before its bytes do: the
-- presign reserves `client_id` and `image_key`, the storage PUT flips
-- `is_uploaded`, and the achievement sync fills in the names and promotes
-- status to 'ready'. The launcher retries the sequence under the same
-- `client_id` until the server acknowledges it, which is what `client_id` is
-- for.
CREATE TABLE souvenirs (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The launcher's idempotency key for one capture, unique per user.
    client_id TEXT NOT NULL,
    -- Official game id (`remoteId` on the launcher side), which the
    -- achievement sync payload keys on.
    remote_game_id TEXT,
    shop TEXT,
    object_id TEXT,
    -- Storage key of the screenshot, also the `imageKey` the launcher holds.
    image_key TEXT NOT NULL,
    is_uploaded INTEGER NOT NULL DEFAULT 0,
    size_in_bytes INTEGER NOT NULL DEFAULT 0,
    -- 'pending' until the achievement sync claims it, then 'ready'.
    status TEXT NOT NULL DEFAULT 'pending',
    -- The achievement that triggered the capture; upper-cased, like the list.
    primary_achievement_name TEXT,
    -- JSON array of the upper-cased names captured together.
    achievement_names TEXT NOT NULL DEFAULT '[]',
    -- Launcher clock, epoch milliseconds, kept verbatim so the two never
    -- disagree by a timezone.
    captured_at INTEGER NOT NULL,
    -- One picture. users.souvenirs_visibility gates the whole tab.
    visibility TEXT NOT NULL DEFAULT 'PUBLIC',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_souvenirs_client ON souvenirs (user_id, client_id);
CREATE INDEX idx_souvenirs_profile ON souvenirs (user_id, status, captured_at DESC);
CREATE INDEX idx_souvenirs_game ON souvenirs (user_id, shop, object_id);

-- The launcher rotates its client id and re-uploads when a key is already
-- spoken for, so one image to one souvenir has to be enforced, not assumed.
CREATE UNIQUE INDEX idx_souvenirs_image_key ON souvenirs (image_key);

-- Keyed by the viewer, not the owner: any member may like another's souvenir.
CREATE TABLE souvenir_likes (
    souvenir_id TEXT NOT NULL REFERENCES souvenirs(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (souvenir_id, user_id)
);

CREATE INDEX idx_souvenir_likes_user ON souvenir_likes (user_id);

-- Deliberately NOT foreign-keyed to souvenirs: the usual outcome of a report
-- is the picture being deleted, and a moderation record that vanishes with its
-- subject is no record at all.
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

-- A second report from the same person is accepted and ignored, so the
-- launcher's "reported" state survives a retry.
CREATE UNIQUE INDEX idx_souvenir_reports_unique
    ON souvenir_reports (souvenir_id, reporter_user_id);

CREATE INDEX idx_souvenir_reports_created ON souvenir_reports (created_at DESC);

-- For the hourly rate limit: the unique index leads with souvenir_id and the
-- one above spans every reporter, so neither serves it.
CREATE INDEX idx_souvenir_reports_reporter_created
    ON souvenir_reports (reporter_user_id, created_at);

-- Account-level privacy, which the official profile owns and the launcher
-- mirrors here so this server can answer for other viewers. 'PRIVATE' until
-- the launcher says otherwise, so nothing is exposed before we're told.
ALTER TABLE users ADD COLUMN souvenirs_visibility TEXT NOT NULL DEFAULT 'PRIVATE';
