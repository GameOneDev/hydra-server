-- Launcher 4.1.6 tags an emulator save state with what produced it (emulator,
-- core, version, host platform) so a restore can put it back where it loads.
-- Stored as the launcher sent it, as JSON; NULL for every other file.
ALTER TABLE cloud_save_snapshot_files ADD COLUMN state_metadata TEXT;
