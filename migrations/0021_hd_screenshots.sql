-- 1080p copies of screenshots, for accounts without a Hydra Cloud subscription.
--
-- Official Hydra owns the screenshot itself: the record, its likes, comments
-- and moderation. It stores a 720p image for an account with no subscription
-- and a 1080p one for a subscriber. For the accounts that keep their cloud
-- storage here instead, the launcher also uploads a 1080p copy to this server
-- and, when it shows a screenshot, prefers that copy over official's 720p.
--
-- So a row is never a screenshot of its own. It is an image attached to an
-- official screenshot id, which only the uploader can write: the key is the
-- owner plus that id, and a lookup asks for both, so one member cannot plant a
-- picture under another member's screenshot.
CREATE TABLE hd_screenshots (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The official Hydra screenshot this is the sharper copy of.
    screenshot_id TEXT NOT NULL,
    -- images/screenshots-hd/<user>/<uuid>.<ext>; served publicly under an
    -- unguessable name like every other image here.
    storage_key TEXT NOT NULL,
    width INTEGER NOT NULL DEFAULT 0,
    height INTEGER NOT NULL DEFAULT 0,
    size_in_bytes INTEGER NOT NULL DEFAULT 0,
    -- Mirrors the official screenshot's visibility, so making a screenshot
    -- private there stops this copy being handed to other members too.
    -- 'FRIENDS' reads as "members of this server", as for souvenirs.
    visibility TEXT NOT NULL DEFAULT 'PRIVATE',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, screenshot_id)
);

CREATE UNIQUE INDEX idx_hd_screenshots_key ON hd_screenshots (storage_key);
