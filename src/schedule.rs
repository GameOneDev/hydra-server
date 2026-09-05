use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::jobs::{self, Job};
use crate::state::AppState;
use crate::triggers::{self, Trigger, MAX_TRIGGERS};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::Row;

const TICK_SECONDS: u64 = 30;

const RUN_HISTORY: i64 = 50;

const FIRST_RUN_DELAY_MINUTES: i64 = 5;

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

fn task_from_row(row: &sqlx::sqlite::SqliteRow) -> Option<Task> {
    let id: String = row.get("id");
    Some(Task {
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

fn parse_triggers(id: &str, stored: String) -> Vec<Trigger> {
    let Ok(values) = serde_json::from_str::<Vec<Value>>(&stored) else {
        tracing::warn!("task {id} has unreadable triggers, treating it as on-demand");
        return Vec::new();
    };

    values
        .into_iter()
        .filter_map(
            |value| match serde_json::from_value::<Trigger>(value.clone()) {
                Ok(trigger) => Some(trigger),
                Err(err) => {
                    tracing::warn!(
                        "task {id} has a trigger this build can't read ({err}): {value}"
                    );
                    None
                }
            },
        )
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

pub async fn run(state: &AppState, id: &str, reason: Reason) -> ApiResult<Value> {
    let task = get(state, id).await?;

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

    let next = task
        .enabled
        .then(|| task.next_timer_run(finished).map(|at| at.to_rfc3339()))
        .flatten();

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
        (Reason::Manual, _) => {
            Event::admin("admin.task.run", format!("Ran {title} by hand — {summary}"))
        }
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

fn running(state: &AppState) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
    state
        .running_tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn is_running(state: &AppState, id: &str) -> bool {
    running(state).contains(id)
}

struct Claim {
    state: AppState,
    id: String,
}

impl Claim {
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

struct Recorded {
    reason: Reason,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
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
                    recent_event(state, kinds, last_run, now)
                        .await
                        .map(Reason::Event)
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
                            Reason::Condition(format!("{} — {measured} now", trigger.label()))
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

async fn recent_event(
    state: &AppState,
    kinds: &[String],
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<String> {
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

async fn due(state: &AppState, now: DateTime<Utc>) -> Result<Vec<(String, Reason)>, sqlx::Error> {
    let mut pending = Vec::new();

    for task in list(state).await? {
        if let Some(reason) = asking_to_run(state, &task, now).await {
            pending.push((task.job.id.to_string(), reason));
        }
    }

    Ok(pending)
}

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

    #[tokio::test]
    async fn every_scheduled_job_runs_against_a_real_database() {
        let server = TestServer::start().await;

        for job in jobs::JOBS.iter().filter(|job| job.schedulable) {
            let result = run(&server.state, job.id, Reason::Manual)
                .await
                .unwrap_or_else(|err| panic!("{} failed: {}", job.id, err.message));

            assert!(
                result["summary"]
                    .as_str()
                    .is_some_and(|line| !line.is_empty()),
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

    #[tokio::test]
    async fn a_restart_leaves_an_edited_schedule_alone() {
        let server = TestServer::start().await;

        update(&server.state, "prune-events", Some(false), None)
            .await
            .expect("switching it off");
        server
            .set("gc-blobs", vec![every(6, Unit::Hour, None)])
            .await;

        ensure_rows(&server.state).await.expect("the second start");

        let pruning = get(&server.state, "prune-events").await.expect("the task");
        assert!(!pruning.enabled);
        assert!(
            pruning.next_run_at.is_none(),
            "a disabled task has no next run"
        );

        let collecting = get(&server.state, "gc-blobs").await.expect("the task");
        assert_eq!(collecting.triggers, vec![every(6, Unit::Hour, None)]);
        assert!(collecting.next_run_at.is_some());
    }

    #[tokio::test]
    async fn only_enabled_tasks_that_are_due_come_up() {
        let server = TestServer::start().await;

        assert!(due(&server.state, Utc::now())
            .await
            .expect("the queue")
            .is_empty());

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

    #[tokio::test]
    async fn a_startup_trigger_fires_once_per_boot() {
        let server = TestServer::start().await;
        server
            .set("vacuum", vec![Trigger::Startup { delay_minutes: 0 }])
            .await;

        assert!(matches!(
            server.asking("vacuum").await,
            Some(Reason::Startup)
        ));

        run(&server.state, "vacuum", Reason::Startup)
            .await
            .expect("the compaction");

        assert!(
            server.asking("vacuum").await.is_none(),
            "it already ran for this boot"
        );
    }

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

        assert!(
            server.asking("gc-blobs").await.is_none(),
            "the backup hasn't run"
        );

        run(&server.state, "backup", Reason::Manual)
            .await
            .expect("a backup");
        assert!(matches!(
            server.asking("gc-blobs").await,
            Some(Reason::After(_))
        ));

        run(
            &server.state,
            "gc-blobs",
            Reason::After("backup".to_string()),
        )
        .await
        .expect("the collection");
        assert!(server.asking("gc-blobs").await.is_none());
    }

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

        assert!(
            server.asking("prune-events").await.is_none(),
            "nothing has expired"
        );

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
        assert!(
            detail.contains('5'),
            "the reason says what it saw: {detail}"
        );

        run(&server.state, "prune-events", Reason::Condition(detail))
            .await
            .expect("the prune");

        assert!(
            server.asking("prune-events").await.is_none(),
            "the prune fixed what the condition was watching"
        );
    }

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

    #[tokio::test]
    async fn a_schedule_the_server_cannot_keep_is_refused() {
        let server = TestServer::start().await;
        let before = get(&server.state, "vacuum")
            .await
            .expect("the task")
            .triggers;

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

        assert!(
            update(&server.state, "delete-orphan-files", Some(true), None)
                .await
                .is_err(),
            "a job that takes arguments has no schedule to edit"
        );

        assert_eq!(
            get(&server.state, "vacuum")
                .await
                .expect("the task")
                .triggers,
            before
        );
    }

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

        let next = task
            .next_run_at
            .as_deref()
            .and_then(parse_time)
            .expect("a next run");
        assert!(next <= Utc::now() + Duration::hours(6) + Duration::minutes(1));

        run(&server.state, "backup", Reason::Manual)
            .await
            .expect("a backup");
        assert!(matches!(
            server.asking("gc-blobs").await,
            Some(Reason::After(_))
        ));
    }
}
