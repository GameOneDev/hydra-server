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

const DEFAULT_RUN_LIMIT: i64 = 20;

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

async fn vocabulary(state: &AppState) -> ApiResult<Value> {
    let mut metrics = Vec::new();
    for metric in Metric::ALL {
        let mut value = metric.json();
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

async fn list(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let tasks = schedule::list(&state).await?;

    Ok(Json(json!({
        "tasks": tasks
            .iter()
            .map(|task| task.json(schedule::is_running(&state, task.job.id)))
            .collect::<Vec<_>>(),
        "now": chrono::Utc::now().to_rfc3339(),
        "vocabulary": vocabulary(&state).await?,
    })))
}

async fn task_json(state: &AppState, id: &str) -> ApiResult<Value> {
    let task = schedule::get(state, id).await?;
    Ok(task.json(schedule::is_running(state, id)))
}

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
    triggers: Option<Vec<Trigger>>,
}

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

async fn runs(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> ApiResult<Json<Value>> {
    schedule::get(&state, &id)
        .await
        .map_err(|_| ApiError::not_found(format!("no scheduled task named {id}")))?;

    Ok(Json(json!({
        "runs": schedule::runs(&state, &id, query.limit.unwrap_or(DEFAULT_RUN_LIMIT)).await?,
    })))
}
