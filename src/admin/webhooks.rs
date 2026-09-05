//! Managing outbound webhooks from the panel.

use super::AdminSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/webhooks", get(list).post(create))
        .route(
            "/admin/api/webhooks/{id}",
            get(detail).put(update).delete(remove),
        )
        .route("/admin/api/webhooks/{id}/test", post(test))
}

fn webhook_json(row: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": row.get::<String, _>("id"),
        "label": row.get::<String, _>("label"),
        "url": row.get::<String, _>("url"),
        "format": row.get::<String, _>("format"),
        /* The secret itself never leaves the server — only whether one is
           set, which is all the panel needs to render. */
        "hasSecret": row.get::<Option<String>, _>("secret").is_some_and(|value| !value.is_empty()),
        "kinds": serde_json::from_str::<Value>(&row.get::<String, _>("kinds")).unwrap_or(json!([])),
        "minSeverity": row.get::<String, _>("min_severity"),
        "enabled": row.get::<i64, _>("enabled") != 0,
        "createdAt": row.get::<String, _>("created_at"),
        "lastDeliveryAt": row.get::<Option<String>, _>("last_delivery_at"),
        "lastStatus": row.get::<Option<String>, _>("last_status"),
        "lastError": row.get::<Option<String>, _>("last_error"),
        "deliveredCount": row.get::<i64, _>("delivered_count"),
        "failureCount": row.get::<i64, _>("failure_count"),
    })
}

async fn list(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let rows = sqlx::query("SELECT * FROM webhooks ORDER BY created_at DESC")
        .fetch_all(&state.pool)
        .await?;

    Ok(Json(json!({
        "webhooks": rows.iter().map(webhook_json).collect::<Vec<_>>(),
        /* The kinds present in the log, so the form can offer real filters
           instead of asking the operator to guess identifiers. */
        "kinds": sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT substr(kind, 1, instr(kind, '.')) FROM events
             WHERE instr(kind, '.') > 0 ORDER BY 1"
        )
        .fetch_all(&state.pool)
        .await?,
    })))
}

async fn detail(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row = sqlx::query("SELECT * FROM webhooks WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("webhook not found"))?;

    Ok(Json(webhook_json(&row)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookRequest {
    #[serde(default)]
    label: Option<String>,
    url: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    min_severity: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn validate(request: &WebhookRequest) -> ApiResult<(String, String)> {
    let url = request.url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ApiError::bad_request(
            "the URL must start with http:// or https://",
        ));
    }

    let format = request.format.clone().unwrap_or_else(|| "json".to_string());
    if !matches!(format.as_str(), "json" | "discord" | "slack") {
        return Err(ApiError::bad_request(
            "format must be json, discord or slack",
        ));
    }

    Ok((url, format))
}

async fn create(
    State(state): State<AppState>,
    _admin: AdminSession,
    Json(request): Json<WebhookRequest>,
) -> ApiResult<Json<Value>> {
    let (url, format) = validate(&request)?;
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO webhooks (id, label, url, format, secret, kinds, min_severity, enabled, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(request.label.clone().unwrap_or_default())
    .bind(&url)
    .bind(&format)
    .bind(request.secret.clone().filter(|secret| !secret.is_empty()))
    .bind(serde_json::to_string(&request.kinds.clone().unwrap_or_default()).unwrap_or_else(|_| "[]".into()))
    .bind(request.min_severity.clone().unwrap_or_else(|| "info".into()))
    .bind(request.enabled.unwrap_or(true) as i64)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    crate::events::record(
        &state,
        Event::admin("admin.webhook.created", format!("Webhook added for {url}"))
            .detail(json!({ "id": id, "url": url, "format": format })),
    )
    .await;

    detail(State(state), AdminSession { expires_at: 0 }, Path(id)).await
}

async fn update(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(request): Json<WebhookRequest>,
) -> ApiResult<Json<Value>> {
    let (url, format) = validate(&request)?;

    let result = sqlx::query(
        "UPDATE webhooks SET
            label = ?, url = ?, format = ?, kinds = ?, min_severity = ?, enabled = ?,
            /* An omitted secret keeps the stored one; an empty string clears it. */
            secret = CASE WHEN ?8 IS NULL THEN secret WHEN ?8 = '' THEN NULL ELSE ?8 END,
            /* Re-enabling by hand is also a fresh start for the failure count. */
            failure_count = CASE WHEN ?6 = 1 THEN 0 ELSE failure_count END
         WHERE id = ?9",
    )
    .bind(request.label.clone().unwrap_or_default())
    .bind(&url)
    .bind(&format)
    .bind(
        serde_json::to_string(&request.kinds.clone().unwrap_or_default())
            .unwrap_or_else(|_| "[]".into()),
    )
    .bind(
        request
            .min_severity
            .clone()
            .unwrap_or_else(|| "info".into()),
    )
    .bind(request.enabled.unwrap_or(true) as i64)
    .bind(&id)
    .bind(request.secret.clone())
    .bind(&id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("webhook not found"));
    }

    crate::events::record(
        &state,
        Event::admin(
            "admin.webhook.updated",
            format!("Webhook updated for {url}"),
        )
        .detail(json!({ "id": id })),
    )
    .await;

    detail(State(state), AdminSession { expires_at: 0 }, Path(id)).await
}

async fn remove(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let url: Option<String> = sqlx::query_scalar("SELECT url FROM webhooks WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    let url = url.ok_or_else(|| ApiError::not_found("webhook not found"))?;

    sqlx::query("DELETE FROM webhooks WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    crate::events::record(
        &state,
        Event::admin(
            "admin.webhook.deleted",
            format!("Webhook removed for {url}"),
        )
        .detail(json!({ "id": id })),
    )
    .await;

    Ok(Json(json!({ "ok": true })))
}

/// POST /admin/api/webhooks/{id}/test — send one now and report what came
/// back, because "saved successfully" says nothing about whether it works.
async fn test(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    match crate::webhooks::test(&state, &id).await {
        Ok(result) => Ok(Json(result)),
        Err(error) => Err(ApiError::bad_request(format!("delivery failed: {error}"))),
    }
}
