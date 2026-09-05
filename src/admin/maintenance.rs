//! Deleting the files the integrity scan flagged, and the inventory export.
//! Everything that runs unattended is a [`crate::jobs`] job instead.

use super::AdminSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::storage;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/api/maintenance/delete-orphan-files",
            post(delete_orphans),
        )
        .route("/admin/api/maintenance/export", get(export))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeleteRequest {
    #[serde(default)]
    keys: Option<Vec<String>>,
}

async fn delete_orphans(
    State(state): State<AppState>,
    _admin: AdminSession,
    body: Option<Json<DeleteRequest>>,
) -> ApiResult<Json<Value>> {
    let keys = body.map(|Json(body)| body).unwrap_or_default().keys;
    let result = delete_orphan_files(&state, keys).await?;

    tracing::info!("admin: deleted orphaned files");

    crate::events::record(
        &state,
        Event::admin(
            "admin.maintenance",
            result["summary"]
                .as_str()
                .unwrap_or("Deleted orphaned files")
                .to_string(),
        )
        .detail(json!({ "action": "delete-orphan-files", "result": result })),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "action": "delete-orphan-files",
        "result": result,
    })))
}

/// Deletes files the integrity scan flagged as unreferenced.
///
/// The keys come from the scan rather than from the caller's imagination:
/// each one is re-checked against the database here, so a stale panel tab can
/// never delete a file that has since been claimed.
async fn delete_orphan_files(state: &AppState, keys: Option<Vec<String>>) -> ApiResult<Value> {
    let keys = keys.unwrap_or_default();
    if keys.is_empty() {
        return Err(ApiError::bad_request(
            "no files given — run the integrity scan first",
        ));
    }

    let mut deleted = 0usize;
    let mut freed = 0u64;
    let mut skipped: Vec<String> = Vec::new();

    for key in keys {
        if !is_safe_relative_key(&key) || still_referenced(state, &key).await? {
            skipped.push(key);
            continue;
        }

        let path = storage::storage_path(state, &key);
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            freed += meta.len();
        }
        storage::delete_object(state, &key).await;
        deleted += 1;
    }

    Ok(json!({
        "summary": format!("Deleted {deleted} orphaned file(s)."),
        "deleted": deleted,
        "freedBytes": freed,
        "skipped": skipped,
    }))
}

/// Same shape check the storage layer applies to signed keys: relative, no
/// traversal, no absolute paths.
fn is_safe_relative_key(key: &str) -> bool {
    !key.is_empty()
        && !key.contains("..")
        && !key.starts_with('/')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Re-derives ownership for a storage key straight from the database.
async fn still_referenced(state: &AppState, key: &str) -> ApiResult<bool> {
    if let Some(rest) = key.strip_prefix("cloud-saves/") {
        let Some((user_id, hash)) = rest.split_once('/') else {
            return Ok(false);
        };
        let found: Option<String> =
            sqlx::query_scalar("SELECT hash FROM cloud_save_blobs WHERE user_id = ? AND hash = ?")
                .bind(user_id)
                .bind(hash)
                .fetch_optional(&state.pool)
                .await?;
        return Ok(found.is_some());
    }

    if let Some(id) = key
        .strip_prefix("artifacts/")
        .and_then(|rest| rest.strip_suffix(".tar"))
    {
        let found: Option<String> = sqlx::query_scalar("SELECT id FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
        return Ok(found.is_some());
    }

    if let Some(id) = key
        .strip_prefix("emulation-saves/")
        .and_then(|rest| rest.strip_suffix(".bin"))
    {
        let found: Option<String> =
            sqlx::query_scalar("SELECT id FROM emulation_saves WHERE id = ?")
                .bind(id)
                .fetch_optional(&state.pool)
                .await?;
        return Ok(found.is_some());
    }

    if key.starts_with("images/artwork/") {
        let found: Option<String> =
            sqlx::query_scalar("SELECT storage_key FROM game_artwork WHERE storage_key = ?")
                .bind(key)
                .fetch_optional(&state.pool)
                .await?;
        return Ok(found.is_some());
    }

    if key.starts_with("images/banners/") || key.starts_with("images/avatars/") {
        /* The live file is the one on the user's row. `profile_image_url` is
        consulted too: it is all an avatar uploaded before `avatar_key`
        existed has, and mistaking one of those for an orphan would delete
        a picture someone is still using. */
        let found: Option<String> = sqlx::query_scalar(
            "SELECT id FROM users
             WHERE banner_key = ?1 OR avatar_key = ?1
                OR profile_image_url LIKE ?2 ESCAPE '\\'",
        )
        .bind(key)
        .bind(format!("%{}", escape_like(key)))
        .fetch_optional(&state.pool)
        .await?;
        return Ok(found.is_some());
    }

    /* Anything else is outside the areas the scan reconciles — refuse rather
    than delete something this code doesn't understand. */
    Ok(true)
}

/// GET /admin/api/maintenance/export — the whole inventory as JSON.
///
/// Not a backup of the save data (that is the storage directory), but of what
/// the server believes it holds: enough to answer questions off-line, diff two
/// points in time, or hand to someone debugging a sync.
async fn export(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let users = sqlx::query(
        "SELECT id, username, display_name, is_blocked, created_at, last_seen_at FROM users",
    )
    .fetch_all(&state.pool)
    .await?;

    let snapshots = sqlx::query(
        "SELECT id, user_id, shop, object_id, version, status, file_count,
                total_size_in_bytes, platform, hostname, created_at, updated_at
         FROM cloud_save_snapshots",
    )
    .fetch_all(&state.pool)
    .await?;

    let artifacts = sqlx::query(
        "SELECT id, user_id, shop, object_id, artifact_length_in_bytes, label, hostname,
                platform, is_frozen, is_uploaded, download_count, created_at
         FROM artifacts",
    )
    .fetch_all(&state.pool)
    .await?;

    let emulation = sqlx::query(
        "SELECT id, user_id, platform, emulator, save_identity, artifact_length_in_bytes,
                file_name, label, is_uploaded, created_at, updated_at
         FROM emulation_saves",
    )
    .fetch_all(&state.pool)
    .await?;

    let settings = state.settings.read().await.clone();

    Ok(Json(json!({
        "exportedAt": Utc::now().to_rfc3339(),
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "publicUrl": state.config.public_url,
            "officialApiUrl": state.config.official_api_url,
        },
        "settings": {
            "maxBytesPerUser": settings.max_bytes_per_user,
            "backupsPerGameLimit": settings.backups_per_game_limit,
            "autoDeleteSaves": settings.auto_delete_saves,
            "allowedUsers": settings.allowed_users,
        },
        "users": users.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "username": row.get::<Option<String>, _>("username"),
            "displayName": row.get::<String, _>("display_name"),
            "isBlocked": row.get::<i64, _>("is_blocked") != 0,
            "createdAt": row.get::<String, _>("created_at"),
            "lastSeenAt": row.get::<String, _>("last_seen_at"),
        })).collect::<Vec<_>>(),
        "cloudSaveSnapshots": snapshots.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "userId": row.get::<String, _>("user_id"),
            "shop": row.get::<String, _>("shop"),
            "objectId": row.get::<String, _>("object_id"),
            "version": row.get::<i64, _>("version"),
            "status": row.get::<String, _>("status"),
            "fileCount": row.get::<i64, _>("file_count"),
            "sizeBytes": row.get::<i64, _>("total_size_in_bytes"),
            "platform": row.get::<Option<String>, _>("platform"),
            "hostname": row.get::<Option<String>, _>("hostname"),
            "createdAt": row.get::<String, _>("created_at"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
        "backups": artifacts.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "userId": row.get::<String, _>("user_id"),
            "shop": row.get::<String, _>("shop"),
            "objectId": row.get::<String, _>("object_id"),
            "sizeBytes": row.get::<i64, _>("artifact_length_in_bytes"),
            "label": row.get::<Option<String>, _>("label"),
            "hostname": row.get::<String, _>("hostname"),
            "platform": row.get::<Option<String>, _>("platform"),
            "isFrozen": row.get::<i64, _>("is_frozen") != 0,
            "isUploaded": row.get::<i64, _>("is_uploaded") != 0,
            "downloadCount": row.get::<i64, _>("download_count"),
            "createdAt": row.get::<String, _>("created_at"),
        })).collect::<Vec<_>>(),
        "emulationSaves": emulation.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "userId": row.get::<String, _>("user_id"),
            "platform": row.get::<String, _>("platform"),
            "emulator": row.get::<String, _>("emulator"),
            "saveIdentity": row.get::<String, _>("save_identity"),
            "sizeBytes": row.get::<i64, _>("artifact_length_in_bytes"),
            "fileName": row.get::<Option<String>, _>("file_name"),
            "label": row.get::<Option<String>, _>("label"),
            "isUploaded": row.get::<i64, _>("is_uploaded") != 0,
            "createdAt": row.get::<String, _>("created_at"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
    })))
}
