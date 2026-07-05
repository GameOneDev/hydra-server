use crate::auth::CurrentUser;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSource {
    pub id: String,
    pub url: String,
    pub name: Option<String>,
    pub created_at: String,
}

/// GET /profile/download-sources — sources synced across the user's devices.
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<DownloadSource>>> {
    let rows = sqlx::query(
        "SELECT id, url, name, created_at FROM download_sources
         WHERE user_id = ? ORDER BY created_at ASC",
    )
    .bind(&user.0.id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|row| DownloadSource {
                id: row.get("id"),
                url: row.get("url"),
                name: row.get("name"),
                created_at: row.get("created_at"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct AddSources {
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// POST /profile/download-sources { urls: [...] }
pub async fn add(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<AddSources>,
) -> ApiResult<StatusCode> {
    let now = Utc::now().to_rfc3339();

    for url in payload.urls.iter().filter(|url| !url.trim().is_empty()) {
        sqlx::query(
            "INSERT INTO download_sources (id, user_id, url, name, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, url) DO NOTHING",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&user.0.id)
        .bind(url.trim())
        .bind(&payload.name)
        .bind(&now)
        .execute(&state.pool)
        .await?;
    }

    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveQuery {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub remove_all: Option<String>,
}

/// DELETE /profile/download-sources?url=…|id=…|removeAll=true
pub async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    if matches!(query.remove_all.as_deref(), Some("true" | "1")) {
        sqlx::query("DELETE FROM download_sources WHERE user_id = ?")
            .bind(&user.0.id)
            .execute(&state.pool)
            .await?;
        return Ok(StatusCode::OK);
    }

    if let Some(id) = &query.id {
        sqlx::query("DELETE FROM download_sources WHERE user_id = ? AND id = ?")
            .bind(&user.0.id)
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    if let Some(url) = &query.url {
        sqlx::query("DELETE FROM download_sources WHERE user_id = ? AND url = ?")
            .bind(&user.0.id)
            .bind(url)
            .execute(&state.pool)
            .await?;
    }

    Ok(StatusCode::OK)
}
