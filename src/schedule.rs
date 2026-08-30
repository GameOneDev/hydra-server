//! The maintenance schedule: which jobs run unattended, when, and what
//! happened the last few times they did.
//!
//! The premise of this server is that it is one binary you start, not a
//! binary plus a cron entry — so the timetable lives here, in-process and in
//! the database, where the panel can show it and change it. Every entry is a
//! [`crate::jobs::Job`]; the operator picks the cadence, the time of day and
//! whether it runs at all, and each run is recorded with what it changed.
//!
//! Times are UTC throughout. A minute-of-day is a plain integer rather than a
//! local wall clock: the server has no timezone to speak of, and a schedule
//! that shifts under a daylight-saving change is a schedule nobody can
//! predict. The panel shows both UTC and the reader's local time.

use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::jobs::{self, Job};
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::Row;

/// How often the timer looks for due work. Fine enough that "every 15
/// minutes" means it, cheap enough to be one indexed query.
const TICK_SECONDS: u64 = 30;

/// Runs kept per task. The panel shows the most recent handful; the rest is
/// there for "when did this start failing", which a couple of dozen answers.
const RUN_HISTORY: i64 = 50;

/// A first run shortly after start rather than at the next scheduled time, so
/// a new server — or one that just upgraded into this — does its housekeeping
/// straight away instead of waiting a day to prove the schedule works.
const FIRST_RUN_DELAY_MINUTES: i64 = 5;

pub const MIN_INTERVAL_MINUTES: i64 = 15;
pub const MAX_INTERVAL_MINUTES: i64 = 30 * 24 * 60;

pub const MINUTES_PER_DAY: i64 = 24 * 60;

/// The cadences the panel offers. Any interval in range is accepted by the
/// API; these are the ones with a name.
pub const FREQUENCIES: &[i64] = &[
    30,
    60,
    3 * 60,
    6 * 60,
    12 * 60,
    MINUTES_PER_DAY,
    2 * MINUTES_PER_DAY,
    7 * MINUTES_PER_DAY,
    30 * MINUTES_PER_DAY,
];

/// One job's schedule and its last outcome.
pub struct Task {
    pub job: &'static Job,
    pub enabled: bool,
    pub interval_minutes: i64,
    /// Minute of the day, UTC. Only meaningful for whole-day intervals.
    pub at_minute: Option<i64>,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_status: Option<String>,
    pub last_summary: Option<String>,
    pub last_duration_ms: Option<i64>,
}

impl Task {
    /// True when the time of day is part of this task's schedule. A cadence
    /// shorter than a day just runs every interval from the last run.
    pub fn times_of_day(&self) -> bool {
        self.interval_minutes % MINUTES_PER_DAY == 0
    }

    pub fn json(&self, running: bool) -> Value {
        let mut value = self.job.json();
        let at_minute = self.times_of_day().then_some(self.at_minute).flatten();

        value["enabled"] = json!(self.enabled);
        value["intervalMinutes"] = json!(self.interval_minutes);
        value["atMinute"] = json!(at_minute);
        value["frequencyLabel"] = json!(frequency_label(self.interval_minutes));
        value["scheduleLabel"] = json!(schedule_label(self.enabled, self.interval_minutes, at_minute));
        /* What it would do if it were on, so a paused task's card can say so
           rather than only saying "off". */
        value["cadenceLabel"] = json!(schedule_label(true, self.interval_minutes, at_minute));
        value["nextRunAt"] = json!(self.next_run_at);
        value["running"] = json!(running);
        value["lastRunAt"] = json!(self.last_run_at);
        value["lastStatus"] = json!(self.last_status);
        value["lastSummary"] = json!(self.last_summary);
        value["lastDurationMs"] = json!(self.last_duration_ms);
        value
    }
}

/// "every 6 hours", "every day" — the phrase the screen puts next to a task.
pub fn frequency_label(minutes: i64) -> String {
    let plural = |count: i64, unit: &str| match count {
        1 => format!("every {unit}"),
        n => format!("every {n} {unit}s"),
    };

    match minutes {
        m if m % (7 * MINUTES_PER_DAY) == 0 => plural(m / (7 * MINUTES_PER_DAY), "week"),
        m if m % MINUTES_PER_DAY == 0 => plural(m / MINUTES_PER_DAY, "day"),
        m if m % 60 == 0 => plural(m / 60, "hour"),
        m => plural(m, "minute"),
    }
}

/// The whole schedule in one line: "every day at 03:00 UTC", "off".
pub fn schedule_label(enabled: bool, minutes: i64, at_minute: Option<i64>) -> String {
    if !enabled {
        return "off".to_string();
    }
    match at_minute {
        Some(at) => format!("{} at {} UTC", frequency_label(minutes), clock(at)),
        None => frequency_label(minutes),
    }
}

/// A minute of the day as `HH:MM`.
pub fn clock(at_minute: i64) -> String {
    let at = at_minute.rem_euclid(MINUTES_PER_DAY);
    format!("{:02}:{:02}", at / 60, at % 60)
}

/// When a task with this cadence next comes due after `from`.
///
/// With a time of day, runs land on that minute and the interval steps whole
/// days from it, so "every day at 03:00" is 03:00 every day rather than 24
/// hours after whenever the last run happened to finish. Without one, the
/// interval simply runs from now.
pub fn next_run(from: DateTime<Utc>, interval_minutes: i64, at_minute: Option<i64>) -> DateTime<Utc> {
    let interval = interval_minutes.clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES);

    let Some(at) = at_minute.filter(|_| interval % MINUTES_PER_DAY == 0) else {
        return from + Duration::minutes(interval);
    };

    let midnight = from.date_naive().and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
    let mut candidate = midnight + Duration::minutes(at.rem_euclid(MINUTES_PER_DAY));
    while candidate <= from {
        candidate += Duration::minutes(interval);
    }
    candidate
}

fn task_from_row(row: &sqlx::sqlite::SqliteRow) -> Option<Task> {
    let id: String = row.get("id");
    Some(Task {
        /* A row whose job no longer exists — an id removed in an upgrade —
           is skipped rather than rendered: nothing can run it. */
        job: jobs::find(&id)?,
        enabled: row.get::<i64, _>("enabled") != 0,
        interval_minutes: row.get("interval_minutes"),
        at_minute: row.get("at_minute"),
        next_run_at: row.get("next_run_at"),
        last_run_at: row.get("last_run_at"),
        last_status: row.get("last_status"),
        last_summary: row.get("last_summary"),
        last_duration_ms: row.get("last_duration_ms"),
    })
}

/// Creates the row for every schedulable job that hasn't got one, with the
/// defaults from [`Job::default_schedule`]. Called at startup, so a job added
/// in a later release schedules itself without a migration.
pub async fn ensure_rows(state: &AppState) -> Result<(), sqlx::Error> {
    let now = Utc::now();

    for job in jobs::JOBS.iter().filter(|job| job.schedulable) {
        let (enabled, interval, at_minute) = job.default_schedule(&state.config);
        let first_run = enabled.then(|| (now + Duration::minutes(FIRST_RUN_DELAY_MINUTES)).to_rfc3339());

        sqlx::query(
            "INSERT INTO scheduled_tasks
               (id, enabled, interval_minutes, at_minute, next_run_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(job.id)
        .bind(enabled as i64)
        .bind(interval)
        .bind(at_minute)
        .bind(&first_run)
        .bind(now.to_rfc3339())
        .execute(&state.pool)
        .await?;
    }

    Ok(())
}

/// Every schedulable job, in catalogue order, whether or not it has a row
/// yet — a panel that hides a task until the next restart would be lying
/// about what this server does.
pub async fn list(state: &AppState) -> Result<Vec<Task>, sqlx::Error> {
    let rows = sqlx::query("SELECT * FROM scheduled_tasks")
        .fetch_all(&state.pool)
        .await?;

    let mut stored: std::collections::HashMap<&str, Task> = rows
        .iter()
        .filter_map(task_from_row)
        .map(|task| (task.job.id, task))
        .collect();

    Ok(jobs::JOBS
        .iter()
        .filter(|job| job.schedulable)
        .map(|job| {
            stored
                .remove(job.id)
                .unwrap_or_else(|| default_task(job, &state.config))
        })
        .collect())
}

fn default_task(job: &'static Job, config: &crate::config::Config) -> Task {
    let (enabled, interval_minutes, at_minute) = job.default_schedule(config);
    Task {
        job,
        enabled,
        interval_minutes,
        at_minute,
        next_run_at: None,
        last_run_at: None,
        last_status: None,
        last_summary: None,
        last_duration_ms: None,
    }
}

pub async fn get(state: &AppState, id: &str) -> ApiResult<Task> {
    let job = jobs::find(id)
        .filter(|job| job.schedulable)
        .ok_or_else(|| ApiError::not_found(format!("no scheduled task named {id}")))?;

    let row = sqlx::query("SELECT * FROM scheduled_tasks WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;

    Ok(row
        .as_ref()
        .and_then(task_from_row)
        .unwrap_or_else(|| default_task(job, &state.config)))
}

/// Applies an operator's edit and recomputes when the task next comes due.
///
/// Every change re-anchors the next run to now, so moving a daily job from
/// 03:00 to 04:00 takes effect tonight rather than after one more run at the
/// old time.
pub async fn update(
    state: &AppState,
    id: &str,
    enabled: Option<bool>,
    interval_minutes: Option<i64>,
    at_minute: Option<Option<i64>>,
) -> ApiResult<Task> {
    let current = get(state, id).await?;

    let enabled = enabled.unwrap_or(current.enabled);
    let interval = interval_minutes.unwrap_or(current.interval_minutes);
    let at = at_minute.unwrap_or(current.at_minute);

    if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&interval) {
        return Err(ApiError::bad_request(format!(
            "the interval must be between {MIN_INTERVAL_MINUTES} minutes and {} days",
            MAX_INTERVAL_MINUTES / MINUTES_PER_DAY
        )));
    }

    if let Some(minute) = at {
        if !(0..MINUTES_PER_DAY).contains(&minute) {
            return Err(ApiError::bad_request("the run time must be a minute of the day"));
        }
    }

    let now = Utc::now();
    let next = enabled.then(|| next_run(now, interval, at).to_rfc3339());

    sqlx::query(
        "INSERT INTO scheduled_tasks
           (id, enabled, interval_minutes, at_minute, next_run_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           enabled = excluded.enabled,
           interval_minutes = excluded.interval_minutes,
           at_minute = excluded.at_minute,
           next_run_at = excluded.next_run_at,
           updated_at = excluded.updated_at",
    )
    .bind(id)
    .bind(enabled as i64)
    .bind(interval)
    .bind(at)
    .bind(&next)
    .bind(now.to_rfc3339())
    .execute(&state.pool)
    .await?;

    get(state, id).await
}

/// Where a run came from. Only two things start one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Schedule => "schedule",
            Trigger::Manual => "manual",
        }
    }
}

/// Runs a task now, records the run, and re-arms the timer.
///
/// One run of a job at a time, whichever trigger asks: the timer coming due
/// while an operator's "run now" is still going would have two VACUUMs, or
/// two backups a second apart, competing over the same database.
pub async fn run(state: &AppState, id: &str, trigger: Trigger) -> ApiResult<Value> {
    let task = get(state, id).await?;

    /* The claim is held by a guard rather than released by the line after the
       run: a job that panics must not leave itself wedged as "running" until
       the next restart. */
    let Some(_claim) = Claim::take(state, id) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("{} is already running", task.job.title),
        ));
    };

    let started = Utc::now();
    let outcome = jobs::run(state, id, trigger.as_str()).await;
    let finished = Utc::now();

    let (status, summary, detail) = match &outcome {
        Ok(result) => (
            "ok",
            result["summary"].as_str().unwrap_or("Done").to_string(),
            result.clone(),
        ),
        Err(error) => (
            "error",
            error.message.clone(),
            json!({ "error": error.message }),
        ),
    };

    let recorded = Recorded {
        trigger,
        started,
        finished,
        status,
        summary,
        detail,
    };
    record_run(state, id, &recorded).await;

    /* The next run is measured from the end of this one whichever trigger
       started it: an operator who runs a daily job by hand at noon should not
       then get the scheduled one an hour later. */
    let next = task
        .enabled
        .then(|| next_run(finished, task.interval_minutes, task.at_minute).to_rfc3339());

    /* Upserted rather than updated: the first thing a fresh server does may
       well be an operator pressing "Run now", and the outcome of that run
       still has to land somewhere. */
    let update = sqlx::query(
        "INSERT INTO scheduled_tasks
           (id, enabled, interval_minutes, at_minute, next_run_at, updated_at,
            last_run_at, last_status, last_summary, last_duration_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           next_run_at = excluded.next_run_at,
           last_run_at = excluded.last_run_at,
           last_status = excluded.last_status,
           last_summary = excluded.last_summary,
           last_duration_ms = excluded.last_duration_ms",
    )
    .bind(id)
    .bind(task.enabled as i64)
    .bind(task.interval_minutes)
    .bind(task.at_minute)
    .bind(&next)
    .bind(finished.to_rfc3339())
    .bind(started.to_rfc3339())
    .bind(recorded.status)
    .bind(&recorded.summary)
    .bind(recorded.duration_ms())
    .execute(&state.pool)
    .await;

    if let Err(err) = update {
        tracing::warn!("failed to record the result of task {id}: {err}");
    }

    let title = task.job.title;
    let summary = &recorded.summary;
    let event = match (trigger, recorded.status) {
        (_, "error") => {
            Event::system("system.task.failed", format!("{title} failed: {summary}")).warning()
        }
        (Trigger::Manual, _) => Event::admin(
            "admin.task.run",
            format!("Ran {title} by hand — {summary}"),
        ),
        _ => Event::system("system.task.ran", format!("{title}: {summary}")),
    };

    crate::events::record(
        state,
        event.detail(json!({
            "task": id,
            "trigger": trigger.as_str(),
            "durationMs": recorded.duration_ms(),
            "result": recorded.detail,
        })),
    )
    .await;

    outcome
}

/// The set of running jobs. A poisoned lock is recovered rather than
/// propagated: it holds nothing but ids, and refusing every future run
/// because one job panicked would be the worse failure.
fn running(state: &AppState) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
    state
        .running_tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether this job is running right now, for the panel.
pub fn is_running(state: &AppState, id: &str) -> bool {
    running(state).contains(id)
}

/// One job's claim on itself, for as long as a run of it is in flight.
struct Claim {
    state: AppState,
    id: String,
}

impl Claim {
    /// Takes the claim, or `None` when something else already holds it.
    fn take(state: &AppState, id: &str) -> Option<Self> {
        running(state).insert(id.to_string()).then(|| Self {
            state: state.clone(),
            id: id.to_string(),
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        running(&self.state).remove(&self.id);
    }
}

/// One run, as it goes into the log and onto the task's row.
struct Recorded {
    trigger: Trigger,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
    /// "ok" | "error"
    status: &'static str,
    summary: String,
    detail: Value,
}

impl Recorded {
    fn duration_ms(&self) -> i64 {
        (self.finished - self.started).num_milliseconds()
    }
}

async fn record_run(state: &AppState, id: &str, run: &Recorded) {
    let insert = sqlx::query(
        "INSERT INTO scheduled_task_runs
           (task_id, trigger, started_at, finished_at, duration_ms, status, summary, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(run.trigger.as_str())
    .bind(run.started.to_rfc3339())
    .bind(run.finished.to_rfc3339())
    .bind(run.duration_ms())
    .bind(run.status)
    .bind(&run.summary)
    .bind(run.detail.to_string())
    .execute(&state.pool)
    .await;

    if let Err(err) = insert {
        tracing::warn!("failed to log the run of task {id}: {err}");
        return;
    }

    /* Trimmed here rather than by a job of its own: the log of a task that
       runs every fifteen minutes is the one that would grow, and it trims
       itself every time it does. */
    let trimmed = sqlx::query(
        "DELETE FROM scheduled_task_runs
          WHERE task_id = ?
            AND id NOT IN (
              SELECT id FROM scheduled_task_runs
               WHERE task_id = ? ORDER BY id DESC LIMIT ?
            )",
    )
    .bind(id)
    .bind(id)
    .bind(RUN_HISTORY)
    .execute(&state.pool)
    .await;

    if let Err(err) = trimmed {
        tracing::warn!("failed to trim the run log of task {id}: {err}");
    }
}

/// The recorded runs of one task, newest first.
pub async fn runs(state: &AppState, id: &str, limit: i64) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT * FROM scheduled_task_runs WHERE task_id = ? ORDER BY started_at DESC LIMIT ?",
    )
    .bind(id)
    .bind(limit.clamp(1, RUN_HISTORY))
    .fetch_all(&state.pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "trigger": row.get::<String, _>("trigger"),
                "startedAt": row.get::<String, _>("started_at"),
                "finishedAt": row.get::<String, _>("finished_at"),
                "durationMs": row.get::<i64, _>("duration_ms"),
                "status": row.get::<String, _>("status"),
                "summary": row.get::<String, _>("summary"),
                "detail": row
                    .get::<Option<String>, _>("detail")
                    .and_then(|detail| serde_json::from_str::<Value>(&detail).ok()),
            })
        })
        .collect())
}

/// The tasks the timer would start right now.
async fn due(state: &AppState, now: DateTime<Utc>) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id FROM scheduled_tasks
          WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?
          ORDER BY next_run_at",
    )
    .bind(now.to_rfc3339())
    .fetch_all(&state.pool)
    .await?;

    /* Only ids this build knows how to run: a row left behind by a job that
       was removed must not be picked up every tick forever. */
    Ok(rows
        .iter()
        .map(|row| row.get::<String, _>("id"))
        .filter(|id| jobs::find(id).is_some_and(|job| job.schedulable))
        .collect())
}

/// The timer. One tick every [`TICK_SECONDS`], running whatever is due, in
/// sequence — two heavy jobs coming due in the same minute should queue
/// rather than fight over the database.
///
/// A run missed while the server was down fires once on the next tick after
/// it comes back, not once per missed period: the next time is stored, not
/// derived from a count.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        if let Err(err) = ensure_rows(&state).await {
            tracing::warn!("failed to create the schedule: {err}");
        }

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));

        loop {
            ticker.tick().await;

            let ids = match due(&state, Utc::now()).await {
                Ok(ids) => ids,
                Err(err) => {
                    tracing::warn!("failed to read the schedule: {err}");
                    continue;
                }
            };

            for id in ids {
                match run(&state, &id, Trigger::Schedule).await {
                    Ok(result) => tracing::info!(
                        "scheduled task {id}: {}",
                        result["summary"].as_str().unwrap_or("done")
                    ),
                    Err(error) => tracing::warn!("scheduled task {id} failed: {}", error.message),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A server on a scratch database, so the schedule is exercised against
    /// the tables it actually uses rather than a stand-in for them.
    struct TestServer {
        state: AppState,
        dir: PathBuf,
    }

    impl TestServer {
        async fn start() -> Self {
            let dir = std::env::temp_dir().join(format!("hydra-schedule-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("scratch directory");

            let mut config = crate::config::Config::for_test();
            config.data_dir = dir.clone();

            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(config.database_path())
                        .create_if_missing(true),
                )
                .await
                .expect("scratch database");

            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("migrations");

            let settings = crate::state::RuntimeSettings::from_config(&config);

            let state = AppState {
                pool,
                config: Arc::new(config),
                http: reqwest::Client::new(),
                token_cache: Default::default(),
                settings: Arc::new(tokio::sync::RwLock::new(settings)),
                started_at: Utc::now(),
                metrics: Default::default(),
                uploads: Default::default(),
                login_guard: Default::default(),
                running_tasks: Default::default(),
                presence: Default::default(),
            };

            Self { state, dir }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn at(time: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(time)
            .expect("a timestamp")
            .with_timezone(&Utc)
    }

    /// "Every day at 03:00" has to mean 03:00 — not "24 hours after whenever
    /// the last run finished", which drifts an hour further into the morning
    /// every time a run takes a minute.
    #[test]
    fn a_daily_task_lands_on_its_time_of_day() {
        let day = MINUTES_PER_DAY;

        assert_eq!(
            next_run(at("2026-08-30T14:20:00Z"), day, Some(180)),
            at("2026-08-31T03:00:00Z"),
        );
        /* Before today's slot: today, not tomorrow. */
        assert_eq!(
            next_run(at("2026-08-30T02:59:00Z"), day, Some(180)),
            at("2026-08-30T03:00:00Z"),
        );
        /* Exactly on it: the next one, so a run can never re-trigger itself. */
        assert_eq!(
            next_run(at("2026-08-30T03:00:00Z"), day, Some(180)),
            at("2026-08-31T03:00:00Z"),
        );
        /* Weekly steps whole weeks from that same time of day. */
        assert_eq!(
            next_run(at("2026-08-30T05:00:00Z"), 7 * day, Some(4 * 60 + 30)),
            at("2026-09-06T04:30:00Z"),
        );
    }

    /// A cadence shorter than a day has no time of day to land on, so it runs
    /// an interval from now — and says so rather than pretending otherwise.
    #[test]
    fn a_sub_daily_task_runs_an_interval_from_now() {
        assert_eq!(
            next_run(at("2026-08-30T14:20:00Z"), 360, Some(180)),
            at("2026-08-30T20:20:00Z"),
        );
        assert_eq!(schedule_label(true, 360, None), "every 6 hours");
        assert_eq!(schedule_label(true, 1440, Some(180)), "every day at 03:00 UTC");
        assert_eq!(schedule_label(true, 7 * 1440, Some(0)), "every week at 00:00 UTC");
        assert_eq!(schedule_label(false, 1440, Some(180)), "off");
        assert_eq!(frequency_label(30), "every 30 minutes");
    }

    /// Every job the schedule offers has to survive a real run against a real
    /// database — the timer has nobody to report a panic to.
    #[tokio::test]
    async fn every_scheduled_job_runs_against_a_real_database() {
        let server = TestServer::start().await;
        ensure_rows(&server.state).await.expect("the schedule");

        for job in jobs::JOBS.iter().filter(|job| job.schedulable) {
            let result = run(&server.state, job.id, Trigger::Manual)
                .await
                .unwrap_or_else(|err| panic!("{} failed: {}", job.id, err.message));

            assert!(
                result["summary"].as_str().is_some_and(|line| !line.is_empty()),
                "{} reported nothing",
                job.id
            );

            let task = get(&server.state, job.id).await.expect("the task");
            assert_eq!(task.last_status.as_deref(), Some("ok"), "{}", job.id);
            assert_eq!(runs(&server.state, job.id, 10).await.expect("its log").len(), 1);
        }
    }

    /// The defaults are seeded once. A restart must not re-arm a task the
    /// operator switched off, nor move one they retimed.
    #[tokio::test]
    async fn a_restart_leaves_an_edited_schedule_alone() {
        let server = TestServer::start().await;
        ensure_rows(&server.state).await.expect("the schedule");

        update(&server.state, "prune-events", Some(false), None, None)
            .await
            .expect("switching it off");
        update(&server.state, "gc-blobs", None, Some(6 * 60), Some(None))
            .await
            .expect("retiming it");

        ensure_rows(&server.state).await.expect("the second start");

        let pruning = get(&server.state, "prune-events").await.expect("the task");
        assert!(!pruning.enabled);
        assert!(pruning.next_run_at.is_none(), "a disabled task has no next run");

        let collecting = get(&server.state, "gc-blobs").await.expect("the task");
        assert_eq!(collecting.interval_minutes, 360);
        assert!(collecting.next_run_at.is_some());
    }

    /// What the timer picks up: enabled, due, and nothing else.
    #[tokio::test]
    async fn only_enabled_tasks_that_are_due_come_up() {
        let server = TestServer::start().await;
        ensure_rows(&server.state).await.expect("the schedule");

        /* Seeded a few minutes out, so nothing is due yet. */
        assert!(due(&server.state, Utc::now()).await.expect("the queue").is_empty());

        let due_now = due(&server.state, Utc::now() + Duration::hours(1))
            .await
            .expect("the queue");
        assert!(due_now.contains(&"prune-events".to_string()));
        assert!(
            !due_now.contains(&"vacuum".to_string()),
            "compaction is off by default and must stay off"
        );

        update(&server.state, "prune-events", Some(false), None, None)
            .await
            .expect("switching it off");
        assert!(!due(&server.state, Utc::now() + Duration::hours(1))
            .await
            .expect("the queue")
            .contains(&"prune-events".to_string()));
    }

    /// Two runs of the same job at once would have two VACUUMs, or two
    /// backups a second apart, competing over one database — so the second
    /// one is refused, and the claim is released however the first ends.
    #[tokio::test]
    async fn a_job_only_runs_once_at_a_time() {
        let server = TestServer::start().await;
        ensure_rows(&server.state).await.expect("the schedule");

        {
            let _held = Claim::take(&server.state, "vacuum").expect("the first claim");
            assert!(is_running(&server.state, "vacuum"));
            assert!(Claim::take(&server.state, "vacuum").is_none());

            let refusal = run(&server.state, "vacuum", Trigger::Manual)
                .await
                .expect_err("something else is already compacting");
            assert_eq!(refusal.status, StatusCode::CONFLICT);
        }

        /* The guard is gone, so the job is runnable again. */
        assert!(!is_running(&server.state, "vacuum"));
        run(&server.state, "vacuum", Trigger::Manual)
            .await
            .expect("the claim was released");
    }

    /// An interval nobody could have meant is refused rather than stored: a
    /// one-minute VACUUM loop is a broken server, not a fast schedule.
    #[tokio::test]
    async fn the_cadence_has_to_be_one_a_server_can_keep() {
        let server = TestServer::start().await;
        ensure_rows(&server.state).await.expect("the schedule");

        assert!(update(&server.state, "vacuum", None, Some(1), None).await.is_err());
        assert!(update(&server.state, "vacuum", None, Some(400 * 24 * 60), None).await.is_err());
        assert!(update(&server.state, "vacuum", None, None, Some(Some(1500))).await.is_err());
        assert!(update(&server.state, "delete-orphan-files", Some(true), None, None)
            .await
            .is_err(), "a job that takes arguments has no schedule to edit");

        /* And the edit that was refused changed nothing. */
        let task = get(&server.state, "vacuum").await.expect("the task");
        assert_eq!(task.interval_minutes, 7 * MINUTES_PER_DAY);
    }
}
