use crate::artifacts::GameArtifact;
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

/// A user a backup has been shared with, for the owner's management UI.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactShare {
    pub id: String,
    pub recipient_id: String,
    pub recipient_display_name: Option<String>,
    pub recipient_profile_image_url: Option<String>,
    pub created_at: String,
}

/// A backup shared with the current user, including who shared it and which
/// game it belongs to.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedGameArtifact {
    #[serde(flatten)]
    pub artifact: GameArtifact,
    pub shop: String,
    pub object_id: String,
    pub shared_at: String,
    pub shared_by: SharedBy,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedBy {
    pub id: String,
    pub display_name: String,
    pub profile_image_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareArtifact {
    pub recipient_id: String,
}

/// POST /profile/games/artifacts/{id}/share — share one of your backups with
/// another user (the launcher restricts the picker to your friend list).
pub async fn share(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
    Json(payload): Json<ShareArtifact>,
) -> ApiResult<StatusCode> {
    let recipient_id = payload.recipient_id.trim().to_string();

    if recipient_id.is_empty() {
        return Err(ApiError::bad_request("recipient is required"));
    }

    if recipient_id == user.0.id {
        return Err(ApiError::bad_request("cannot share a backup with yourself"));
    }

    let owns_artifact: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM artifacts WHERE id = ? AND user_id = ? AND is_uploaded = 1",
    )
    .bind(&id)
    .bind(&user.0.id)
    .fetch_optional(&state.pool)
    .await?;

    if owns_artifact.is_none() {
        return Err(ApiError::not_found("artifact not found"));
    }

    sqlx::query(
        "INSERT INTO artifact_shares (id, artifact_id, owner_user_id, recipient_user_id, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(artifact_id, recipient_user_id) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&id)
    .bind(&user.0.id)
    .bind(&recipient_id)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    Ok(StatusCode::OK)
}

/// DELETE /profile/games/artifacts/{id}/share/{recipient_id} — the owner can
/// revoke any share; a recipient can remove a share from themselves.
pub async fn unshare(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, recipient_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "DELETE FROM artifact_shares
         WHERE artifact_id = ?
           AND recipient_user_id = ?
           AND (owner_user_id = ? OR recipient_user_id = ?)",
    )
    .bind(&id)
    .bind(&recipient_id)
    .bind(&user.0.id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("share not found"));
    }

    Ok(StatusCode::OK)
}

/// GET /profile/games/artifacts/{id}/shares — who a backup is shared with.
pub async fn list_shares(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ArtifactShare>>> {
    let rows = sqlx::query(
        "SELECT s.id, s.recipient_user_id, s.created_at,
                u.display_name, u.profile_image_url
         FROM artifact_shares s
         LEFT JOIN users u ON u.id = s.recipient_user_id
         WHERE s.artifact_id = ? AND s.owner_user_id = ?
         ORDER BY s.created_at ASC",
    )
    .bind(&id)
    .bind(&user.0.id)
    .fetch_all(&state.pool)
    .await?;

    let shares = rows
        .iter()
        .map(|row| ArtifactShare {
            id: row.get("id"),
            recipient_id: row.get("recipient_user_id"),
            recipient_display_name: row.get("display_name"),
            recipient_profile_image_url: row.get("profile_image_url"),
            created_at: row.get("created_at"),
        })
        .collect();

    Ok(Json(shares))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedWithMeQuery {
    pub shop: Option<String>,
    pub object_id: Option<String>,
}

/// GET /profile/games/artifacts/shared-with-me?shop=&objectId=
pub async fn shared_with_me(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<SharedWithMeQuery>,
) -> ApiResult<Json<Vec<SharedGameArtifact>>> {
    let rows = sqlx::query(
        "SELECT a.*, s.created_at AS shared_at,
                u.id AS owner_id, u.display_name AS owner_display_name,
                u.profile_image_url AS owner_profile_image_url
         FROM artifact_shares s
         JOIN artifacts a ON a.id = s.artifact_id
         JOIN users u ON u.id = s.owner_user_id
         WHERE s.recipient_user_id = ?
           AND a.is_uploaded = 1
           AND (? IS NULL OR a.shop = ?)
           AND (? IS NULL OR a.object_id = ?)
         ORDER BY s.created_at DESC",
    )
    .bind(&user.0.id)
    .bind(&query.shop)
    .bind(&query.shop)
    .bind(&query.object_id)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    let artifacts = rows
        .iter()
        .map(|row| SharedGameArtifact {
            artifact: crate::artifacts::artifact_from_row(row),
            shop: row.get("shop"),
            object_id: row.get("object_id"),
            shared_at: row.get("shared_at"),
            shared_by: SharedBy {
                id: row.get("owner_id"),
                display_name: row.get("owner_display_name"),
                profile_image_url: row.get("owner_profile_image_url"),
            },
        })
        .collect();

    Ok(Json(artifacts))
}
