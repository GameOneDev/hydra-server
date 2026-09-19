-- Which games the Steam integration brought in, so a profile filtered to the
-- Steam library can be answered from here. The launcher sends the flag with
-- every achievement sync; NULL means it has not synced that game since this
-- column existed, and the game is counted as not imported.
ALTER TABLE game_achievements ADD COLUMN has_active_steam_import INTEGER;
