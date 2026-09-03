-- What makes a task run, beyond a timer.
--
-- 0013 gave every task one cadence: an interval, plus a time of day for the
-- long ones. That answers "run this nightly" and nothing else — an operator
-- who wants the sweep to run *when uploads pile up*, or the blob collection
-- to follow every backup, had no way to say so.
--
-- A task now carries a list of triggers, any one of which can start it: a
-- timer, the server starting, another task finishing, an event being
-- recorded, or a measured number crossing a line. The list is JSON rather
-- than a table of its own — it is always read and written whole, one row at a
-- time, and a trigger's fields differ per kind.
ALTER TABLE scheduled_tasks ADD COLUMN triggers TEXT NOT NULL DEFAULT '[]';

-- Carry the existing cadences over, so a server that already set one keeps
-- exactly the schedule it had. Whole days become a daily/weekly trigger with
-- its time of day; anything shorter becomes an hour or minute cadence.
UPDATE scheduled_tasks
   SET triggers = CASE
     WHEN interval_minutes % 10080 = 0 THEN
       json_array(json_object(
         'type', 'every', 'count', interval_minutes / 10080, 'unit', 'week',
         'atMinute', COALESCE(at_minute, 0)))
     WHEN interval_minutes % 1440 = 0 THEN
       json_array(json_object(
         'type', 'every', 'count', interval_minutes / 1440, 'unit', 'day',
         'atMinute', COALESCE(at_minute, 0)))
     WHEN interval_minutes % 60 = 0 THEN
       json_array(json_object(
         'type', 'every', 'count', interval_minutes / 60, 'unit', 'hour'))
     ELSE
       json_array(json_object(
         'type', 'every', 'count', interval_minutes, 'unit', 'minute'))
   END;

ALTER TABLE scheduled_tasks DROP COLUMN interval_minutes;
ALTER TABLE scheduled_tasks DROP COLUMN at_minute;

-- Why a run happened, in the words the panel shows: "every day at 03:00 UTC",
-- "1,204 events past the retention window". The kind alone ('condition') says
-- what sort of trigger fired; this says what it saw.
ALTER TABLE scheduled_task_runs ADD COLUMN reason TEXT;
