-- Per-user exceptions to the server-wide limits. NULL = use the server's value.
ALTER TABLE users ADD COLUMN max_bytes_override INTEGER;
ALTER TABLE users ADD COLUMN backups_per_game_override INTEGER;
ALTER TABLE users ADD COLUMN auto_delete_saves_override INTEGER;

-- Set on the slot's older save when automatic save deletion is off, instead of
-- deleting it. Cloud Save V2 uses the 'superseded' snapshot status for this.
ALTER TABLE emulation_saves ADD COLUMN superseded_at TEXT;
