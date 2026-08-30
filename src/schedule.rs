//! The maintenance schedule: which jobs run unattended, what starts them, and
//! what happened the last few times they did.
//!
//! The premise of this server is that it is one binary you start, not a binary
//! plus a cron entry — so the timetable lives here, in-process and in the
//! database, where the panel can show it and change it. Every entry is a
//! [`crate::jobs::Job`] carrying a list of [`Trigger`]s, and any one of them
//! firing runs the job: a timer, the server starting, another task finishing,
//! an event being recorded, or a measured number crossing a line.
//!
//! Times are UTC throughout. A minute-of-day is a plain integer rather than a
//! local wall clock: the server has no timezone to speak of, and a schedule
//! that shifts under a daylight-saving change is a schedule nobody can
//! predict. The panel shows both UTC and the reader's local time.

use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::jobs::{self, Job};
use crate::state::AppState;
use crate::triggers::{self, Trigger, MAX_TRIGGERS};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::Row;

/// How often the timer looks for due work. Fine enough that a five-minute
/// cadence means it, cheap enough to be a handful of indexed queries.
const TICK_SECONDS: u64 = 30;

/// Runs kept per task. The panel shows the most recent handful; the rest is
/// there for "when did this start failing", which a couple of dozen answers.
const RUN_HISTORY: i64 = 50;

/// A first run shortly after start rather than at the next scheduled time, so
/// a new server — or one that just upgraded into this — does its housekeeping
/// straight away instead of waiting a day to prove the schedule works.
const FIRST_RUN_DELAY_MINUTES: i64 = 5;

/// One job's triggers and its last outcome.
pub struct Task {
    pub job: &'static Job,
    pub enabled: bool,
    pub triggers: Vec<Trigger>,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_status: Option<String>,
    pub last_summary: Option<String>,
    pub last_duration_ms: Option<i64>,
}

impl Task {
    /// The whole schedule in one line, for a list that has no room for more.
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "off".to_string();
        }
        if self.triggers.is_empty() {
            return "on demand only".to_string();
        }
        self.triggers
            .iter()
            .map(Trigger::label)
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The soonest a timer will start this, across every timer it carries.
    fn next_timer_run(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.triggers
            .iter()
            .filter_map(|trigger| trigger.next_run(from))
            .min()
    }

    pub fn json(&self, running: bool) -> Value {
        let mut value = self.job.json();

        value["enabled"] = json!(self.enabled);
        value["triggers"] = json!(self.triggers.iter().map(Trigger::json).collect::<Vec<_>>());
        value["summary"] = json!(self.summary());
        value["nextRunAt"] = json!(self.next_run_at);
        value["running"] = json!(running);
        value["lastRunAt"] = json!(self.last_run_at);
        value["lastStatus"] = json!(self.last_status);
        value["lastSummary"] = json!(self.last_summary);
        value["lastDurationMs"] = json!(self.last_duration_ms);
        value
    }
}

/// What started a run. Recorded with each one, so a log line says whether the
/// clock, the server, another task, an event or an operator was behind it.
#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    Timer(String),
    Startup,
    After(String),
    Event(String),
    Condition(String),
    Manual,
}

impl Reason {
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::Timer(_) => "timer",
            Reason::Startup => "startup",
            Reason::After(_) => "after",
            Reason::Event(_) => "event",
            Reason::Condition(_) => "condition",
            Reason::Manual => "manual",
        }
    }

    /// What the trigger actually saw, in the words the log prints.
    pub fn detail(&self) -> String {
        match self {
            Reason::Timer(label) => label.clone(),
            Reason::Startup => "the server started".to_string(),
            Reason::After(label) => label.clone(),
            Reason::Event(label) => label.clone(),
            Reason::Condition(label) => label.clone(),
            Reason::Manual => "an operator asked for it".to_string(),
        }
    }
}

// ---------------------------------------------------------------- storage

fn task_from_row(row: &sqlx::sqlite::SqliteRow) -> Option<Task> {
    let id: String = row.get("id");
    Some(Task {
        /* A row whose job no longer exists — an id removed in an upgrade — is
           skipped rather than rendered: nothing can run it. */
        job: jobs::find(&id)?,
        enabled: row.get::<i64, _>("enabled") != 0,
        triggers: parse_triggers(&id, row.get::<String, _>("triggers")),
        next_run_at: row.get("next_run_at"),
        last_run_at: row.get("last_run_at"),
        last_status: row.get("last_status"),
        last_summary: row.get("last_summary"),
        last_duration_ms: row.get("last_duration_ms"),
    })
}

/// Triggers this build understands. One it doesn't — a schedule written by a
/// newer version — is dropped with a warning rather than failing the load, so
/// a downgrade costs a trigger and not the whole screen.
fn parse_triggers(id: &str, stored: String) -> Vec<Trigger> {
    let Ok(values) = serde_json::from_str::<Vec<Value>>(&stored) else {
        tracing::warn!("task {id} has unreadable triggers, treating it as on-demand");
        return Vec::new();
    };

    values
        .into_iter()
        .filter_map(|value| match serde_json::from_value::<Trigger>(value.clone()) {
            Ok(trigger) => Some(trigger),
            Err(err) => {
                tracing::warn!("task {id} has a trigger this build can't read ({err}): {value}");
                None
            }
        })
        .collect()
}

fn encode(triggers: &[Trigger]) -> String {
    serde_json::to_string(triggers).unwrap_or_else(|_| "[]".to_string())
}

fn default_task(job: &'static Job, config: &crate::config::Config) -> Task {
    let (enabled, triggers) = job.default_schedule(config);
    Task {
        job,
        enabled,
        triggers,
        next_run_at: None,
        last_run_at: None,
        last_status: None,
        last_summary: None,
        last_duration_ms: None,
    }
}

/// Creates the row for every schedulable job that hasn't got one, with the
/// defaults from [`Job::default_schedule`]. Called at startup, so a job added
/// in a later release schedules itself without a migration.
pub async fn ensure_rows(state: &AppState) -> Result<(), sqlx::Error> {
    let now = Utc::now();

    for job in jobs::JOBS.iter().filter(|job| job.schedulable) {
        let (enabled, triggers) = job.default_schedule(&state.config);
        let first_run =
            enabled.then(|| (now + Duration::minutes(FIRST_RUN_DELAY_MINUTES)).to_rfc3339());

        sqlx::query(
            "INSERT INTO scheduled_tasks (id, enabled, triggers, next_run_at, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(job.id)
        .bind(enabled as i64)
        .bind(encode(&triggers))
        .bind(&first_run)
        .bind(now.to_rfc3339())
        .execute(&state.pool)
        .await?;
    }

    Ok(())
}

/// Every schedulable job, in catalogue order, whether or not it has a row
/// yet — a panel that hides a task until the next restart would be lying about
/// what this server does.
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

/// Applies an operator's edit and recomputes when a timer next comes due.
///
/// Every change re-anchors the timers to now, so moving a daily job from 03:00
/// to 04:00 takes effect tonight rather than after one more run at the old
/// time.
pub async fn update(
    state: &AppState,
    id: &str,
    enabled: Option<bool>,
    triggers: Option<Vec<Trigger>>,
) -> ApiResult<Task> {
    let current = get(state, id).await?;

    let enabled = enabled.unwrap_or(current.enabled);
    let triggers = triggers.unwrap_or(current.triggers);

    if triggers.len() > MAX_TRIGGERS {
        return Err(ApiError::bad_request(format!(
            "a task can carry at most {MAX_TRIGGERS} triggers"
        )));
    }
    for trigger in &triggers {
        trigger.validate(id).map_err(ApiError::bad_request)?;
    }

    let now = Utc::now();
    let pending = Task {
        job: current.job,
        enabled,
        triggers,
        next_run_at: None,
        last_run_at: current.last_run_at,
        last_status: current.last_status,
        last_summary: current.last_summary,
        last_duration_ms: current.last_duration_ms,
    };
    let next = enabled
        .then(|| pending.next_timer_run(now).map(|at| at.to_rfc3339()))
        .flatten();

    sqlx::query(
        "INSERT INTO scheduled_tasks (id, enabled, triggers, next_run_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           enabled = excluded.enabled,
           triggers = excluded.triggers,
           next_run_at = excluded.next_run_at,
           updated_at = excluded.updated_at",
    )
    .bind(id)
    .bind(enabled as i64)
    .bind(encode(&pending.triggers))
    .bind(&next)
    .bind(now.to_rfc3339())
    .execute(&state.pool)
    .await?;

    get(state, id).await
}

// -------------------------------------------------------------- running

/// Runs a task now, records the run, and re-arms its timers.
///
/// One run of a job at a time, whichever trigger asks: a timer coming due
/// while an operator's "run now" is still going would have two VACUUMs, or two
/// backups a second apart, competing over the same database.
pub async fn run(state: &AppState, id: &str, reason: Reason) -> ApiResult<Value> {
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
    let outcome = jobs::run(state, id, reason.kind()).await;
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
        reason,
        started,
        finished,
        status,
        summary,
        detail,
    };
    record_run(state, id, &recorded).await;

    /* The next timer run is measured from the end of this one whichever
       trigger started it: an operator who runs a daily job by hand at noon
       should not then get the scheduled one an hour later. */
    let next = task
        .enabled
        .then(|| task.next_timer_run(finished).map(|at| at.to_rfc3339()))
        .flatten();

    /* Upserted rather than updated: the first thing a fresh server does may
       well be an operator pressing "Run now", and the outcome of that run
       still has to land somewhere. */
    let update = sqlx::query(
        "INSERT INTO scheduled_tasks
           (id, enabled, triggers, next_run_at, updated_at,
            last_run_at, last_status, last_summary, last_duration_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           next_run_at = excluded.next_run_at,
           last_run_at = excluded.last_run_at,
           last_status = excluded.last_status,
           last_summary = excluded.last_summary,
           last_duration_ms = excluded.last_duration_ms",
    )
    .bind(id)
    .bind(task.enabled as i64)
    .bind(encode(&task.triggers))
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
    let event = match (&recorded.reason, recorded.status) {
        (_, "error") => {
            Event::system("system.task.failed", format!("{title} failed: {summary}")).warning()
        }
        (Reason::Manual, _) => Event::admin(
            "admin.task.run",
            format!("Ran {title} by hand — {summary}"),
        ),
        _ => Event::system("system.task.ran", format!("{title}: {summary}")),
    };

    crate::events::record(
        state,
        event.detail(json!({
            "task": id,
            "trigger": recorded.reason.kind(),
            "reason": recorded.reason.detail(),
            "durationMs": recorded.duration_ms(),
            "result": recorded.detail,
        })),
    )
    .await;

    outcome
}

/// The set of running jobs. A poisoned lock is recovered rather than
/// propagated: it holds nothing but ids, and refusing every future run because
/// one job panicked would be the worse failure.
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
    reason: Reason,
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
           (task_id, trigger, reason, started_at, finished_at, duration_ms, status, summary, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(run.reason.kind())
    .bind(run.reason.detail())
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
       runs every five minutes is the one that would grow, and it trims itself
       every time it does. */
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
                "reason": row.get::<Option<String>, _>("reason"),
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

// ------------------------------------------------------------ the timer

/// Whether any of a task's triggers is asking for a run right now, and which.
///
/// The timer is a stored timestamp; the rest are questions asked of the
/// database each tick — "has that task finished since I last ran", "has one of
/// these events been recorded", "is that number over the line". Polling them
/// rather than hooking into the write path keeps every trigger the same shape
/// and costs one small query each, only for the tasks that declare one.
async fn asking_to_run(state: &AppState, task: &Task, now: DateTime<Utc>) -> Option<Reason> {
    if !task.enabled {
        return None;
    }

    let last_run = task.last_run_at.as_deref().and_then(parse_time);

    for trigger in &task.triggers {
        let fired = match trigger {
            Trigger::Every { .. } => task
                .next_run_at
                .as_deref()
                .and_then(parse_time)
                .is_some_and(|next| next <= now)
                .then(|| Reason::Timer(trigger.label())),

            Trigger::Startup { delay_minutes } => {
                let due = state.started_at + Duration::minutes(*delay_minutes);
                /* Once per boot: a run recorded after this process started is
                   this boot's run. */
                (now >= due && last_run.is_none_or(|last| last < state.started_at))
                    .then_some(Reason::Startup)
            }

            Trigger::AfterTask {
                task: parent,
                delay_minutes,
            } => after_task(state, parent, *delay_minutes, last_run, now).await,

            Trigger::OnEvent {
                kinds,
                min_gap_minutes,
            } => {
                if !gap_elapsed(last_run, *min_gap_minutes, now) {
                    None
                } else {
                    recent_event(state, kinds, last_run, now).await.map(Reason::Event)
                }
            }

            Trigger::Condition {
                metric,
                comparison,
                value,
                min_gap_minutes,
            } => {
                if !gap_elapsed(last_run, *min_gap_minutes, now) {
                    None
                } else {
                    triggers::condition_holds(state, *metric, *comparison, *value)
                        .await
                        .map(|measured| {
                            Reason::Condition(format!(
                                "{} — {measured} now",
                                trigger.label()
                            ))
                        })
                }
            }
        };

        if fired.is_some() {
            return fired;
        }
    }

    None
}

/// Has `parent` finished successfully since this task last ran?
async fn after_task(
    state: &AppState,
    parent: &str,
    delay_minutes: i64,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Reason> {
    let row = sqlx::query("SELECT last_run_at, last_status FROM scheduled_tasks WHERE id = ?")
        .bind(parent)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()?;

    if row.get::<Option<String>, _>("last_status").as_deref() != Some("ok") {
        return None;
    }

    let finished = row
        .get::<Option<String>, _>("last_run_at")
        .as_deref()
        .and_then(parse_time)?;

    let fresh = last_run.is_none_or(|last| finished > last);
    let waited = now >= finished + Duration::minutes(delay_minutes);

    (fresh && waited).then(|| {
        let title = jobs::find(parent).map_or(parent, |job| job.title);
        Reason::After(format!("{title} finished"))
    })
}

/// The most recent event matching any of these kind prefixes, if one landed
/// since this task last ran.
async fn recent_event(
    state: &AppState,
    kinds: &[String],
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<String> {
    /* Without a previous run, look back over the tick rather than over all of
       history: a task added today should not fire for something that happened
       last month. */
    let since = last_run.unwrap_or(now - Duration::seconds(TICK_SECONDS as i64 * 2));

    let mut sql = String::from("SELECT kind, summary FROM events WHERE at > ?");
    if !kinds.is_empty() {
        let clauses = kinds
            .iter()
            .map(|_| "kind LIKE ? ESCAPE '\\'")
            .collect::<Vec<_>>()
            .join(" OR ");
        sql.push_str(&format!(" AND ({clauses})"));
    }
    sql.push_str(" ORDER BY at DESC LIMIT 1");

    let mut query = sqlx::query(&sql).bind(since.to_rfc3339());
    for kind in kinds {
        /* Prefix match: "cloud_save." keeps matching a kind added later. The
           operator's own wildcards are escaped so they stay literal. */
        query = query.bind(format!("{}%", escape_like(kind.trim())));
    }

    let row = query.fetch_optional(&state.pool).await.ok().flatten()?;
    Some(format!(
        "{} — {}",
        row.get::<String, _>("kind"),
        row.get::<String, _>("summary")
    ))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn gap_elapsed(last_run: Option<DateTime<Utc>>, min_gap_minutes: i64, now: DateTime<Utc>) -> bool {
    last_run.is_none_or(|last| now >= last + Duration::minutes(min_gap_minutes.max(0)))
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Everything asking to run right now, in catalogue order.
async fn due(state: &AppState, now: DateTime<Utc>) -> Result<Vec<(String, Reason)>, sqlx::Error> {
    let mut pending = Vec::new();

    for task in list(state).await? {
        if let Some(reason) = asking_to_run(state, &task, now).await {
            pending.push((task.job.id.to_string(), reason));
        }
    }

    Ok(pending)
}

/// The timer. One tick every [`TICK_SECONDS`], running whatever is asking, in
/// sequence — two heavy jobs coming due in the same minute should queue rather
/// than fight over the database.
///
/// A run missed while the server was down fires once on the next tick after it
/// comes back, not once per missed period: the next time is stored, not
/// derived from a count.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        if let Err(err) = ensure_rows(&state).await {
            tracing::warn!("failed to create the schedule: {err}");
        }

        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TICK_SECONDS));

        loop {
            ticker.tick().await;

            let pending = match due(&state, Utc::now()).await {
                Ok(pending) => pending,
                Err(err) => {
                    tracing::warn!("failed to read the schedule: {err}");
                    continue;
                }
            };

            for (id, reason) in pending {
                let kind = reason.kind();
                match run(&state, &id, reason).await {
                    Ok(result) => tracing::info!(
                        "task {id} ran ({kind}): {}",
                        result["summary"].as_str().unwrap_or("done")
                    ),
                    Err(error) => {
                        tracing::warn!("task {id} ({kind}) failed: {}", error.message)
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triggers::{Comparison, Metric, Unit};
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
            let dir =
                std::env::temp_dir().join(format!("hydra-schedule-test-{}", uuid::Uuid::new_v4()));
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

            let server = Self { state, dir };
            ensure_rows(&server.state).await.expect("the schedule");
            server
        }

        /// The reason a task would run right now, if any.
        async fn asking(&self, id: &str) -> Option<Reason> {
            let task = get(&self.state, id).await.expect("the task");
            asking_to_run(&self.state, &task, Utc::now()).await
        }

        async fn set(&self, id: &str, triggers: Vec<Trigger>) {
            update(&self.state, id, Some(true), Some(triggers))
                .await
                .expect("a schedule the server accepts");
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn every(count: i64, unit: Unit, at_minute: Option<i64>) -> Trigger {
        Trigger::Every {
            count,
            unit,
            at_minute,
            weekday: None,
            day: None,
        }
    }

    /// Every job the schedule offers has to survive a real run against a real
    /// database — the timer has nobody to report a panic to.
    #[tokio::test]
    async fn every_scheduled_job_runs_against_a_real_database() {
        let server = TestServer::start().await;

        for job in jobs::JOBS.iter().filter(|job| job.schedulable) {
            let result = run(&server.state, job.id, Reason::Manual)
                .await
                .unwrap_or_else(|err| panic!("{} failed: {}", job.id, err.message));

            assert!(
                result["summary"].as_str().is_some_and(|line| !line.is_empty()),
                "{} reported nothing",
                job.id
            );

            let task = get(&server.state, job.id).await.expect("the task");
            assert_eq!(task.last_status.as_deref(), Some("ok"), "{}", job.id);

            let log = runs(&server.state, job.id, 10).await.expect("its log");
            assert_eq!(log.len(), 1);
            assert_eq!(log[0]["trigger"], "manual");
        }
    }

    /// The defaults are seeded once. A restart must not re-arm a task the
    /// operator switched off, nor move one they retimed.
    #[tokio::test]
    async fn a_restart_leaves_an_edited_schedule_alone() {
        let server = TestServer::start().await;

        update(&server.state, "prune-events", Some(false), None)
            .await
            .expect("switching it off");
        server.set("gc-blobs", vec![every(6, Unit::Hour, None)]).await;

        ensure_rows(&server.state).await.expect("the second start");

        let pruning = get(&server.state, "prune-events").await.expect("the task");
        assert!(!pruning.enabled);
        assert!(pruning.next_run_at.is_none(), "a disabled task has no next run");

        let collecting = get(&server.state, "gc-blobs").await.expect("the task");
        assert_eq!(collecting.triggers, vec![every(6, Unit::Hour, None)]);
        assert!(collecting.next_run_at.is_some());
    }

    /// The timer picks up what is due, and nothing else.
    #[tokio::test]
    async fn only_enabled_tasks_that_are_due_come_up() {
        let server = TestServer::start().await;

        /* Seeded a few minutes out, so nothing is due yet. */
        assert!(due(&server.state, Utc::now()).await.expect("the queue").is_empty());

        let later = due(&server.state, Utc::now() + Duration::hours(1))
            .await
            .expect("the queue");
        assert!(later.iter().any(|(id, _)| id == "prune-events"));
        assert!(
            !later.iter().any(|(id, _)| id == "vacuum"),
            "compaction is off by default and must stay off"
        );
        assert!(matches!(later[0].1, Reason::Timer(_)));

        update(&server.state, "prune-events", Some(false), None)
            .await
            .expect("switching it off");
        assert!(!due(&server.state, Utc::now() + Duration::hours(1))
            .await
            .expect("the queue")
            .iter()
            .any(|(id, _)| id == "prune-events"));
    }

    /// A startup trigger fires once for the boot it belongs to, not on every
    /// tick for the rest of the process's life.
    #[tokio::test]
    async fn a_startup_trigger_fires_once_per_boot() {
        let server = TestServer::start().await;
        server
            .set("vacuum", vec![Trigger::Startup { delay_minutes: 0 }])
            .await;

        assert!(matches!(server.asking("vacuum").await, Some(Reason::Startup)));

        run(&server.state, "vacuum", Reason::Startup)
            .await
            .expect("the compaction");

        assert!(
            server.asking("vacuum").await.is_none(),
            "it already ran for this boot"
        );
    }

    /// A task can follow another one, which is how "collect the blobs the
    /// backup just made stale" gets said.
    #[tokio::test]
    async fn a_task_can_follow_another() {
        let server = TestServer::start().await;
        server
            .set(
                "gc-blobs",
                vec![Trigger::AfterTask {
                    task: "backup".to_string(),
                    delay_minutes: 0,
                }],
            )
            .await;

        assert!(server.asking("gc-blobs").await.is_none(), "the backup hasn't run");

        run(&server.state, "backup", Reason::Manual)
            .await
            .expect("a backup");
        assert!(matches!(server.asking("gc-blobs").await, Some(Reason::After(_))));

        /* Following it once is following it: the same backup must not start
           the collection again on every tick after that. */
        run(&server.state, "gc-blobs", Reason::After("backup".to_string()))
            .await
            .expect("the collection");
        assert!(server.asking("gc-blobs").await.is_none());
    }

    /// A measured number crossing a line is a trigger like any other — and
    /// stops being one as soon as the job it started has fixed it.
    #[tokio::test]
    async fn a_condition_starts_a_task_and_then_stops_asking() {
        let server = TestServer::start().await;
        server
            .set(
                "prune-events",
                vec![Trigger::Condition {
                    metric: Metric::ExpiredEvents,
                    comparison: Comparison::Above,
                    value: 2,
                    min_gap_minutes: 0,
                }],
            )
            .await;

        assert!(server.asking("prune-events").await.is_none(), "nothing has expired");

        let stale = (Utc::now() - Duration::days(400)).to_rfc3339();
        for index in 0..5 {
            sqlx::query(
                "INSERT INTO events (at, kind, category, severity, summary)
                 VALUES (?, 'test.old', 'system', 'info', ?)",
            )
            .bind(&stale)
            .bind(format!("an old event {index}"))
            .execute(&server.state.pool)
            .await
            .expect("an event past the window");
        }

        let Some(Reason::Condition(detail)) = server.asking("prune-events").await else {
            panic!("five events past the window is over a threshold of two");
        };
        assert!(detail.contains('5'), "the reason says what it saw: {detail}");

        run(&server.state, "prune-events", Reason::Condition(detail))
            .await
            .expect("the prune");

        assert!(
            server.asking("prune-events").await.is_none(),
            "the prune fixed what the condition was watching"
        );
    }

    /// An event trigger fires for something recorded since the task last ran,
    /// and respects the gap an operator set on it.
    #[tokio::test]
    async fn an_event_trigger_waits_for_its_kind_and_its_gap() {
        let server = TestServer::start().await;
        server
            .set(
                "sweep-pending",
                vec![Trigger::OnEvent {
                    kinds: vec!["cloud_save.".to_string()],
                    min_gap_minutes: 0,
                }],
            )
            .await;

        crate::events::record(
            &server.state,
            Event::system("system.started", "Server started"),
        )
        .await;
        assert!(
            server.asking("sweep-pending").await.is_none(),
            "a kind it isn't listening for"
        );

        crate::events::record(
            &server.state,
            Event::sync("cloud_save.committed", "alice", "Synced a cloud save"),
        )
        .await;
        let Some(Reason::Event(detail)) = server.asking("sweep-pending").await else {
            panic!("the kind it listens for was recorded");
        };
        assert!(detail.contains("cloud_save.committed"), "{detail}");

        /* A gap the operator set is a floor on how often it may fire. */
        run(&server.state, "sweep-pending", Reason::Event(detail))
            .await
            .expect("the sweep");
        server
            .set(
                "sweep-pending",
                vec![Trigger::OnEvent {
                    kinds: vec!["cloud_save.".to_string()],
                    min_gap_minutes: 120,
                }],
            )
            .await;

        crate::events::record(
            &server.state,
            Event::sync("cloud_save.committed", "alice", "Synced another"),
        )
        .await;
        assert!(
            server.asking("sweep-pending").await.is_none(),
            "it swept two minutes ago; the gap says wait"
        );
    }

    /// Two runs of the same job at once would have two VACUUMs, or two backups
    /// a second apart, competing over one database — so the second one is
    /// refused, and the claim is released however the first ends.
    #[tokio::test]
    async fn a_job_only_runs_once_at_a_time() {
        let server = TestServer::start().await;

        {
            let _held = Claim::take(&server.state, "vacuum").expect("the first claim");
            assert!(is_running(&server.state, "vacuum"));
            assert!(Claim::take(&server.state, "vacuum").is_none());

            let refusal = run(&server.state, "vacuum", Reason::Manual)
                .await
                .expect_err("something else is already compacting");
            assert_eq!(refusal.status, StatusCode::CONFLICT);
        }

        assert!(!is_running(&server.state, "vacuum"));
        run(&server.state, "vacuum", Reason::Manual)
            .await
            .expect("the claim was released");
    }

    /// A schedule the server can't keep is refused when it is saved, and the
    /// refused edit changes nothing.
    #[tokio::test]
    async fn a_schedule_the_server_cannot_keep_is_refused() {
        let server = TestServer::start().await;
        let before = get(&server.state, "vacuum").await.expect("the task").triggers;

        assert!(update(
            &server.state,
            "vacuum",
            None,
            Some(vec![every(1, Unit::Minute, None)])
        )
        .await
        .is_err());

        assert!(update(
            &server.state,
            "vacuum",
            None,
            Some(vec![every(1, Unit::Day, None); MAX_TRIGGERS + 1])
        )
        .await
        .is_err());

        assert!(update(&server.state, "delete-orphan-files", Some(true), None)
            .await
            .is_err(), "a job that takes arguments has no schedule to edit");

        assert_eq!(get(&server.state, "vacuum").await.expect("the task").triggers, before);
    }

    /// Several triggers on one task all count, and the soonest timer decides
    /// the next run.
    #[tokio::test]
    async fn a_task_can_carry_several_triggers() {
        let server = TestServer::start().await;
        server
            .set(
                "gc-blobs",
                vec![
                    every(1, Unit::Day, Some(4 * 60)),
                    every(6, Unit::Hour, None),
                    Trigger::AfterTask {
                        task: "backup".to_string(),
                        delay_minutes: 0,
                    },
                ],
            )
            .await;

        let task = get(&server.state, "gc-blobs").await.expect("the task");
        assert_eq!(task.triggers.len(), 3);
        assert!(task.summary().contains(" · "), "{}", task.summary());

        /* The six-hourly timer is sooner than tomorrow's four o'clock. */
        let next = task.next_run_at.as_deref().and_then(parse_time).expect("a next run");
        assert!(next <= Utc::now() + Duration::hours(6) + Duration::minutes(1));

        /* And the trigger that waits still fires when its task finishes. */
        run(&server.state, "backup", Reason::Manual).await.expect("a backup");
        assert!(matches!(server.asking("gc-blobs").await, Some(Reason::After(_))));
    }
}
