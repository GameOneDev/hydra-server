//! The maintenance schedule, from the panel: what runs unattended, what
//! starts it, whether it worked, and the log of the last few times.

use super::AdminSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::jobs;
use crate::schedule::{self, Reason};
use crate::state::AppState;
use crate::triggers::{Metric, Trigger, Unit, MAX_TRIGGERS, MIN_GAP_MINUTES};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/schedule", get(list))
        .route("/admin/api/schedule/{id}", get(show).put(update))
        .route("/admin/api/schedule/{id}/run", post(run))
        .route("/admin/api/schedule/{id}/runs", get(runs))
}

/// How many past runs a task's log shows before the panel asks for more.
const DEFAULT_RUN_LIMIT: i64 = 20;

/// The event families the trigger editor offers. Prefixes, so one keeps
/// matching a kind added in a later release.
const EVENT_KINDS: &[&str] = &[
    "cloud_save.",
    "backup.",
    "emulation_save.",
    "achievements.",
    "souvenir.",
    "artwork.",
    "user.",
    "auth.",
    "admin.",
    "system.",
];

/// Everything the editor needs to draw a trigger it has never seen: the units
/// an interval counts in, the numbers a condition can watch (with what each
/// one reads right now), the events it can listen for, and the other tasks it
/// can follow.
async fn vocabulary(state: &AppState) -> ApiResult<Value> {
    let mut metrics = Vec::new();
    for metric in Metric::ALL {
        let mut value = metric.json();
        /* The current reading, so a threshold is set against what this server
           actually looks like rather than against a guess. */
        value["now"] = json!(metric.measure(state).await);
        metrics.push(value);
    }

    let units: Vec<Value> = [Unit::Minute, Unit::Hour, Unit::Day, Unit::Week, Unit::Month]
        .iter()
        .map(|unit| {
            json!({
                "unit": unit.as_str(),
                "timeOfDay": unit.has_time_of_day(),
            })
        })
        .collect();

    Ok(json!({
        "units": units,
        "metrics": metrics,
        "eventKinds": EVENT_KINDS,
        "tasks": jobs::JOBS
            .iter()
            .filter(|job| job.schedulable)
            .map(|job| json!({ "id": job.id, "title": job.title }))
            .collect::<Vec<_>>(),
        "limits": {
            "maxTriggers": MAX_TRIGGERS,
            "minGapMinutes": MIN_GAP_MINUTES,
        },
    }))
}

/// GET /admin/api/schedule
async fn list(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let tasks = schedule::list(&state).await?;

    Ok(Json(json!({
        "tasks": tasks
            .iter()
            .map(|task| task.json(schedule::is_running(&state, task.job.id)))
            .collect::<Vec<_>>(),
        /* The clock the schedule is kept in, so the screen can say what
           "03:00" means here and offer the reader's own time beside it. */
        "now": chrono::Utc::now().to_rfc3339(),
        "vocabulary": vocabulary(&state).await?,
    })))
}

async fn task_json(state: &AppState, id: &str) -> ApiResult<Value> {
    let task = schedule::get(state, id).await?;
    Ok(task.json(schedule::is_running(state, id)))
}

/// GET /admin/api/schedule/{id}
async fn show(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "task": task_json(&state, &id).await? })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    enabled: Option<bool>,
    /// The whole list, replacing whatever was stored. A trigger is edited by
    /// sending the list it belongs to, so the screen can never save half of a
    /// change.
    triggers: Option<Vec<Trigger>>,
}

/// PUT /admin/api/schedule/{id} — enable/disable, and set the triggers.
async fn update(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> ApiResult<Json<Value>> {
    let task = schedule::update(&state, &id, request.enabled, request.triggers).await?;
    let summary = task.summary();

    crate::events::record(
        &state,
        Event::admin(
            "admin.schedule.updated",
            format!("{} is now {summary}", task.job.title),
        )
        .detail(json!({
            "task": id,
            "enabled": task.enabled,
            "triggers": task.triggers.iter().map(Trigger::json).collect::<Vec<_>>(),
        })),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "task": task_json(&state, &id).await?,
        "summary": format!("{} — {summary}.", task.job.title),
    })))
}

/// POST /admin/api/schedule/{id}/run — run it now, out of turn.
async fn run(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = schedule::run(&state, &id, Reason::Manual).await?;

    Ok(Json(json!({
        "ok": true,
        "result": result,
        "task": task_json(&state, &id).await?,
    })))
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
}

/// GET /admin/api/schedule/{id}/runs — this task's log.
async fn runs(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> ApiResult<Json<Value>> {
    /* Ask for a task that isn't one and get a 404 rather than an empty log
       that reads like "it has never run". */
    schedule::get(&state, &id)
        .await
        .map_err(|_| ApiError::not_found(format!("no scheduled task named {id}")))?;

    Ok(Json(json!({
        "runs": schedule::runs(&state, &id, query.limit.unwrap_or(DEFAULT_RUN_LIMIT)).await?,
    })))
}
