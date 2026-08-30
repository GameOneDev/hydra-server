-- The recurring jobs, and what happened the last time each one ran.
--
-- Until now the housekeeping was a hard-coded hourly loop: a backup when one
-- was due, a prune of old events, and nothing else — no way to see when it
-- last ran, whether it worked, or to ask for it at a different time. An
-- operator who wanted the sweep or the blob collection on a schedule had to
-- click them by hand.
--
-- One row per job in `jobs::JOBS`. Rows are created by the server rather than
-- here, because a fresh install's defaults depend on the environment (the
-- backup keeps taking its cadence from HYDRA_BACKUP_INTERVAL_HOURS), and a
-- job added in a later release must get its row without a new migration.
CREATE TABLE scheduled_tasks (
    -- The job id from `jobs::JOBS`, e.g. 'backup', 'prune-events'.
    id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 1,
    -- How often it runs. Minutes rather than a cron expression: every cadence
    -- the panel offers is a fixed period, and a period is something an
    -- operator can read off the screen without parsing five fields.
    interval_minutes INTEGER NOT NULL,
    -- Minute of the day, UTC, a run should land on. Honoured when the
    -- interval is a whole number of days — "every day at 03:00" — and NULL
    -- for shorter intervals, which simply run every interval.
    at_minute INTEGER,
    -- When the timer will pick it up next. NULL while disabled.
    next_run_at TEXT,
    -- The last outcome, denormalised from scheduled_task_runs so the screen
    -- can show status for every job in one query.
    last_run_at TEXT,
    -- 'ok' | 'error'
    last_status TEXT,
    last_summary TEXT,
    last_duration_ms INTEGER,
    updated_at TEXT NOT NULL
);

-- The log the panel shows per task. Trimmed to the most recent runs of each
-- job as they are written, so this can never become the biggest table in the
-- database on a server nobody looks at.
CREATE TABLE scheduled_task_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    -- 'schedule' when the timer fired it, 'manual' when an operator did.
    trigger TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    -- 'ok' | 'error'
    status TEXT NOT NULL,
    summary TEXT NOT NULL,
    -- JSON: whatever the job counted, or the error it reported.
    detail TEXT
);

CREATE INDEX idx_task_runs_task_at ON scheduled_task_runs (task_id, started_at DESC);
