use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

/// Response shape matches the launcher's `GameArtifact` type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameArtifact {
    pub id: String,
    pub artifact_length_in_bytes: i64,
    pub download_option_title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub hostname: String,
    pub download_count: i64,
    pub label: Option<String>,
    pub is_frozen: bool,
}

pub(crate) fn artifact_from_row(row: &sqlx::sqlite::SqliteRow) -> GameArtifact {
    GameArtifact {
        id: row.get("id"),
        artifact_length_in_bytes: row.get("artifact_length_in_bytes"),
        download_option_title: row.get("download_option_title"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        hostname: row.get("hostname"),
        download_count: row.get("download_count"),
        label: row.get("label"),
        is_frozen: row.get::<i64, _>("is_frozen") != 0,
    }
}

fn artifact_key(id: &str) -> String {
    format!("artifacts/{id}.tar")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    pub shop: Option<String>,
    pub object_id: Option<String>,
}

/// GET /profile/games/artifacts?shop=&objectId=
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<GameArtifact>>> {
    let rows = sqlx::query(
        "SELECT * FROM artifacts
         WHERE user_id = ?
           AND is_uploaded = 1
           AND (? IS NULL OR shop = ?)
           AND (? IS NULL OR object_id = ?)
         ORDER BY created_at DESC",
    )
    .bind(&user.0.id)
    .bind(&query.shop)
    .bind(&query.shop)
    .bind(&query.object_id)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(rows.iter().map(artifact_from_row).collect()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateArtifact {
    pub artifact_length_in_bytes: i64,
    pub shop: String,
    pub object_id: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub wine_prefix_path: Option<String>,
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default)]
    pub download_option_title: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// POST /profile/games/artifacts -> { id, uploadUrl }
pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<CreateArtifact>,
) -> ApiResult<Json<serde_json::Value>> {
    if payload.artifact_length_in_bytes < 0 {
        return Err(ApiError::bad_request("invalid artifact length"));
    }

    enforce_quotas(&state, &user.0.id, &payload).await?;

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO artifacts (
            id, user_id, shop, object_id, artifact_length_in_bytes, hostname,
            wine_prefix_path, home_dir, download_option_title, platform, label,
            created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user.0.id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .bind(payload.artifact_length_in_bytes)
    .bind(payload.hostname.as_deref().unwrap_or(""))
    .bind(&payload.wine_prefix_path)
    .bind(payload.home_dir.as_deref().unwrap_or(""))
    .bind(&payload.download_option_title)
    .bind(&payload.platform)
    .bind(&payload.label)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    let upload_url = storage::sign_upload_url(
        &state,
        &artifact_key(&id),
        payload.artifact_length_in_bytes as u64,
    );

    Ok(Json(json!({ "id": id, "uploadUrl": upload_url })))
}

async fn enforce_quotas(
    state: &AppState,
    user_id: &str,
    payload: &CreateArtifact,
) -> ApiResult<()> {
    let per_game: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifacts WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .fetch_one(&state.pool)
    .await?;

    if per_game >= state.config.backups_per_game_limit as i64 {
        return Err(ApiError::bad_request(
            "backup limit for this game reached — delete an older backup first",
        ));
    }

    if state.config.max_bytes_per_user > 0 {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;

        let emulation_used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;

        if used + emulation_used + payload.artifact_length_in_bytes
            > state.config.max_bytes_per_user as i64
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "storage quota exceeded — free up space or ask the server admin",
            ));
        }
    }

    Ok(())
}

/// POST /profile/games/artifacts/{id}/download
/// -> { downloadUrl, objectKey, homeDir, winePrefixPath }
///
/// The owner can always download; so can any user the backup was shared with.
pub async fn download(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = sqlx::query(
        "SELECT * FROM artifacts
         WHERE id = ?
           AND is_uploaded = 1
           AND (
             user_id = ?
             OR EXISTS (
               SELECT 1 FROM artifact_shares
               WHERE artifact_id = artifacts.id AND recipient_user_id = ?
             )
           )",
    )
    .bind(&id)
    .bind(&user.0.id)
    .bind(&user.0.id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("artifact not found"))?;

    sqlx::query("UPDATE artifacts SET download_count = download_count + 1 WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    let download_url = storage::sign_download_url(&state, &artifact_key(&id));

    Ok(Json(json!({
        "downloadUrl": download_url,
        /* The launcher joins objectKey onto its userData dir as a temp file
           name, so keep it flat. */
        "objectKey": format!("hydra-artifact-{id}.tar"),
        "homeDir": row.get::<String, _>("home_dir"),
        "winePrefixPath": row.get::<Option<String>, _>("wine_prefix_path"),
    })))
}

/// DELETE /profile/games/artifacts/{id}
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query("DELETE FROM artifacts WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(&user.0.id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    /* Foreign keys are not enforced on this connection, so drop the share
       rows explicitly. */
    sqlx::query("DELETE FROM artifact_shares WHERE artifact_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    storage::delete_object(&state, &artifact_key(&id)).await;

    Ok(StatusCode::OK)
}

/// PUT /profile/games/artifacts/{id}/freeze | /unfreeze
pub async fn set_frozen(
    state: AppState,
    user: CurrentUser,
    id: String,
    frozen: bool,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE artifacts SET is_frozen = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(frozen as i64)
    .bind(Utc::now().to_rfc3339())
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    Ok(StatusCode::OK)
}

pub async fn freeze(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    set_frozen(state, user, id, true).await
}

pub async fn unfreeze(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    set_frozen(state, user, id, false).await
}

#[derive(Deserialize)]
pub struct RenameArtifact {
    pub label: Option<String>,
}

/// PATCH /profile/games/artifacts/{id} — rename a backup.
pub async fn rename(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
    Json(payload): Json<RenameArtifact>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE artifacts SET label = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(&payload.label)
    .bind(Utc::now().to_rfc3339())
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    Ok(StatusCode::OK)
}
