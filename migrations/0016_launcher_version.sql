-- The launcher names itself in the User-Agent of every request it makes
-- ("Hydra Launcher v3.2.1"), so the version each account is running is
-- already arriving here — it was simply thrown away. Recording it answers
-- "who is still on an old build" without asking anyone to look it up, which
-- is the first question when one user's syncs behave unlike everybody else's.
--
-- Nothing to backfill: the header is only on live requests, so a row fills in
-- the next time that account syncs. NULL means "not seen since this landed".
ALTER TABLE users ADD COLUMN launcher_version TEXT;
