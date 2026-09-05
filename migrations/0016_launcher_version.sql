-- The launcher names its version in the User-Agent of every request
-- ("Hydra Launcher v3.2.1"). NULL until an account next syncs.
ALTER TABLE users ADD COLUMN launcher_version TEXT;
