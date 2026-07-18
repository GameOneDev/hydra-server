-- Stored size of an uploaded custom image, so artwork counts toward the
-- per-user storage quota and shows up in the admin panel's totals.
--
-- Zero for SteamGridDB picks, which live on their CDN and cost this server
-- nothing.
ALTER TABLE game_artwork ADD COLUMN size_in_bytes INTEGER NOT NULL DEFAULT 0;
