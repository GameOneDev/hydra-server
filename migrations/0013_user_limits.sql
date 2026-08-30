-- Per-user limits, and the switch that stops the server deleting saves.
--
-- The server-wide limits (`server_settings`, or the environment behind it)
-- stay the default for everyone; these columns are the exception for one
-- account. NULL means "whatever the server says", which is what every
-- existing row gets, so nothing changes until an operator sets something.
ALTER TABLE users ADD COLUMN max_bytes_override INTEGER;
ALTER TABLE users ADD COLUMN backups_per_game_override INTEGER;
ALTER TABLE users ADD COLUMN auto_delete_saves_override INTEGER;

-- Emulation saves are replaced in place: committing a save deletes the older
-- one for the same slot. With automatic deletion off the old row is stamped
-- here instead and kept. The launcher's own listing hides stamped rows, so it
-- still sees exactly one save per slot; the panel and the portal show them so
-- they can be downloaded or deleted by hand.
--
-- Cloud Save V2 needs no column for this: a superseded snapshot is kept with
-- status 'superseded', which the one-committed-snapshot-per-game index allows
-- and every listing already tells apart.
ALTER TABLE emulation_saves ADD COLUMN superseded_at TEXT;
