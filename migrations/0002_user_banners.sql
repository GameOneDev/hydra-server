-- Current banner per user, so launchers can fall back to this server when
-- the official profile has no banner (e.g. free accounts whose banner the
-- official API refused to store).
ALTER TABLE users ADD COLUMN banner_key TEXT;
