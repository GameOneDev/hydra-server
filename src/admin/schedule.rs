//! The maintenance schedule, from the panel: what runs unattended, when it
//! runs, whether it worked, and the log of the last few times.

use super::AdminSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::schedule::{self, Trigger};
use crate::state::AppState;
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

async fn payload(state: &AppState) -> ApiResult<Value> {
    let tasks = schedule::list(state).await?;

    Ok(json!({
        "tasks": tasks
            .iter()
            .map(|task| task.json(schedule::is_running(state, task.job.id)))
            .collect::<Vec<_>>(),
        /* The clock the schedule is kept in, so the screen can say what
           "03:00" means here and offer the reader's own time beside it. */
        "now": chrono::Utc::now().to_rfc3339(),
        "frequencies": schedule::FREQUENCIES
            .iter()
            .map(|minutes| json!({
                "minutes": minutes,
                "label": schedule::frequency_label(*minutes),
                /* Only whole-day cadences land on a time of day; the panel
                   greys the time field out for the others. */
                "timeOfDay": minutes % schedule::MINUTES_PER_DAY == 0,
            }))
            .collect::<Vec<_>>(),
        "limits": {
            "minIntervalMinutes": schedule::MIN_INTERVAL_MINUTES,
            "maxIntervalMinutes": schedule::MAX_INTERVAL_MINUTES,
        },
    }))
}

/// GET /admin/api/schedule
async fn list(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    Ok(Json(payload(&state).await?))
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
    interval_minutes: Option<i64>,
    /// Minute of the day, UTC. Explicit `null` clears the time of day; an
    /// absent field leaves it as it was.
    #[serde(default, deserialize_with = "double_option")]
    at_minute: Option<Option<i64>>,
}

/// Tells "field absent" from "field set to null", which
/// `Option<Option<T>>` alone cannot.
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// PUT /admin/api/schedule/{id} — enable/disable, change the cadence, move
/// the time of day.
async fn update(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> ApiResult<Json<Value>> {
    let task = schedule::update(
        &state,
        &id,
        request.enabled,
        request.interval_minutes,
        request.at_minute,
    )
    .await?;

    let label = schedule::schedule_label(
        task.enabled,
        task.interval_minutes,
        task.times_of_day().then_some(task.at_minute).flatten(),
    );

    crate::events::record(
        &state,
        Event::admin(
            "admin.schedule.updated",
            format!("{} is now {label}", task.job.title),
        )
        .detail(json!({
            "task": id,
            "enabled": task.enabled,
            "intervalMinutes": task.interval_minutes,
            "atMinute": task.at_minute,
        })),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "task": task_json(&state, &id).await?,
        "summary": format!("{} — {label}.", task.job.title),
    })))
}

/// POST /admin/api/schedule/{id}/run — run it now, out of turn.
async fn run(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = schedule::run(&state, &id, Trigger::Manual).await?;

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
