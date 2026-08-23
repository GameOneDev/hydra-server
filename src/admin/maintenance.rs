//! One-shot operations an operator runs by hand.
//!
//! Everything here is something the server would otherwise only do lazily —
//! on the next upload, on the next lookup, on the next restart. Exposing them
//! as buttons turns "wait and hope" into "run it and read the result", and
//! every one reports what it actually changed rather than just succeeding.

use super::AdminSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::{cloud_saves, games, storage};
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/maintenance", get(list))
        .route("/admin/api/maintenance/{action}", post(run))
        .route("/admin/api/maintenance/export", get(export))
}

/// Abandoned uploads older than this are swept — the same threshold the
/// upload path applies on a user's next sync.
const PENDING_TTL_HOURS: i64 = 24;

/// The catalogue the panel renders. Kept here so a new tool is one entry plus
/// one match arm, and the front end needs no edit at all.
fn catalogue() -> Vec<Value> {
    vec![
        json!({
            "id": "sweep-pending",
            "title": "Sweep abandoned uploads",
            "description": format!("Delete cloud save snapshots left pending for more than {PENDING_TTL_HOURS}h. Their blobs are freed with them."),
            "danger": false,
        }),
        json!({
            "id": "gc-blobs",
            "title": "Collect orphaned blobs",
            "description": "Delete Cloud Save V2 blobs no manifest references any more, for every user. Runs automatically after each commit; run it here if a quota looks wrong.",
            "danger": false,
        }),
        json!({
            "id": "delete-orphan-files",
            "title": "Delete orphaned files",
            "description": "Remove files on disk that no database row points at. Review them on the Storage screen first — this cannot be undone.",
            "danger": true,
        }),
        json!({
            "id": "refresh-metadata",
            "title": "Refresh game metadata",
            "description": "Re-resolve names and cover art for games the store lookup never answered for.",
            "danger": false,
        }),
        json!({
            "id": "clear-token-cache",
            "title": "Clear the token cache",
            "description": "Force every launcher token to be re-validated against the official API on its next request.",
            "danger": false,
        }),
        json!({
            "id": "prune-events",
            "title": "Prune old history",
            "description": "Delete recorded events past the retention window. The window is HYDRA_EVENT_RETENTION_DAYS; this runs daily on its own.",
            "danger": false,
        }),
        json!({
            "id": "vacuum",
            "title": "Compact the database",
            "description": "Checkpoint the write-ahead log and VACUUM. Reclaims space after a large delete.",
            "danger": false,
        }),
    ]
}

async fn list(State(_state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "actions": catalogue() })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    /// Restricts `delete-orphan-files` to keys the operator actually saw.
    #[serde(default)]
    keys: Option<Vec<String>>,
}

/// POST /admin/api/maintenance/{action}
async fn run(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(action): Path<String>,
    body: Option<Json<RunRequest>>,
) -> ApiResult<Json<Value>> {
    let request = body.map(|Json(body)| body).unwrap_or_default();

    let result = match action.as_str() {
        "sweep-pending" => sweep_pending(&state).await?,
        "gc-blobs" => gc_blobs(&state).await?,
        "delete-orphan-files" => delete_orphan_files(&state, request.keys).await?,
        "refresh-metadata" => refresh_metadata(&state).await?,
        "clear-token-cache" => {
            let cleared = {
                let mut cache = state.token_cache.write().await;
                let size = cache.len();
                cache.clear();
                size
            };
            json!({ "summary": format!("Dropped {cleared} cached token(s)."), "cleared": cleared })
        }
        "prune-events" => {
            let removed = crate::events::prune(&state, state.config.event_retention_days)
                .await
                .map_err(|err| ApiError::internal(err.to_string()))?;
            json!({
                "summary": match removed {
                    0 => "Nothing past the retention window.".to_string(),
                    n => format!("Pruned {n} event(s)."),
                },
                "pruned": removed,
            })
        }
        "vacuum" => vacuum(&state).await?,
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown maintenance action: {other}"
            )))
        }
    };

    tracing::info!("admin: ran maintenance action {action}");

    crate::events::record(
        &state,
        Event::admin(
            "admin.maintenance",
            result["summary"].as_str().unwrap_or("Maintenance action run").to_string(),
        )
        .detail(json!({ "action": action, "result": result })),
    )
    .await;

    Ok(Json(json!({ "ok": true, "action": action, "result": result })))
}

async fn sweep_pending(state: &AppState) -> ApiResult<Value> {
    let cutoff = (Utc::now() - Duration::hours(PENDING_TTL_HOURS)).to_rfc3339();

    let stale = sqlx::query(
        "SELECT id, user_id FROM cloud_save_snapshots
         WHERE status = 'pending' AND created_at < ?",
    )
    .bind(&cutoff)
    .fetch_all(&state.pool)
    .await?;

    let mut owners: std::collections::HashSet<String> = Default::default();
    for row in &stale {
        let id: String = row.get("id");
        owners.insert(row.get("user_id"));

        sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
            .bind(&id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
            .bind(&id)
            .execute(&state.pool)
            .await?;
    }

    for user_id in &owners {
        cloud_saves::collect_orphan_blobs(state, user_id).await?;
    }

    /* Souvenir captures reserve a row (and sometimes upload bytes) before the
       achievement sync claims them; one that never got claimed is abandoned
       the same way, and the launcher rotates its client id rather than
       resuming, so nothing will ever come back for it. */
    let souvenirs = crate::souvenirs::sweep_abandoned(state, &cutoff).await?;
    let swept = stale.len() + souvenirs;

    Ok(json!({
        "summary": match swept {
            0 => "No abandoned uploads to sweep.".to_string(),
            n => format!(
                "Swept {n} abandoned upload(s): {} cloud save(s) across {} user(s), {souvenirs} souvenir(s).",
                stale.len(),
                owners.len()
            ),
        },
        "swept": swept,
    }))
}

async fn gc_blobs(state: &AppState) -> ApiResult<Value> {
    let users: Vec<String> = sqlx::query_scalar("SELECT DISTINCT user_id FROM cloud_save_blobs")
        .fetch_all(&state.pool)
        .await?;

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_save_blobs")
        .fetch_one(&state.pool)
        .await?;
    let bytes_before: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs")
            .fetch_one(&state.pool)
            .await?;

    for user_id in &users {
        cloud_saves::collect_orphan_blobs(state, user_id).await?;
    }

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_save_blobs")
        .fetch_one(&state.pool)
        .await?;
    let bytes_after: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs")
            .fetch_one(&state.pool)
            .await?;

    Ok(json!({
        "summary": match before - after {
            0 => "Nothing to collect — every blob is still referenced.".to_string(),
            n => format!("Freed {n} blob(s)."),
        },
        "freed": before - after,
        "freedBytes": bytes_before - bytes_after,
    }))
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

/// Re-derives ownership for a storage key straight from the database.
async fn still_referenced(state: &AppState, key: &str) -> ApiResult<bool> {
    if let Some(rest) = key.strip_prefix("cloud-saves/") {
        let Some((user_id, hash)) = rest.split_once('/') else {
            return Ok(false);
        };
        let found: Option<String> = sqlx::query_scalar(
            "SELECT hash FROM cloud_save_blobs WHERE user_id = ? AND hash = ?",
        )
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

    /* Anything else is outside the areas the scan reconciles — refuse rather
       than delete something this code doesn't understand. */
    Ok(true)
}

async fn refresh_metadata(state: &AppState) -> ApiResult<Value> {
    /* Games with data but no resolved name. Bounded: a store lookup is a
       network round trip each, and the panel shouldn't hang on a thousand. */
    let pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT t.shop, t.object_id FROM (
             SELECT shop, object_id FROM cloud_save_snapshots
             UNION SELECT shop, object_id FROM artifacts
             UNION SELECT shop, object_id FROM playtime_daily
             UNION SELECT shop, object_id FROM game_artwork
         ) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         WHERE g.name IS NULL LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut resolved = 0usize;
    for (shop, object_id) in &pending {
        /* resolve() re-fetches only when the cached failure is old enough;
           dropping the row first makes this an explicit retry. */
        sqlx::query("DELETE FROM game_metadata WHERE shop = ? AND object_id = ?")
            .bind(shop)
            .bind(object_id)
            .execute(&state.pool)
            .await?;

        if games::resolve(state, shop, object_id).await.name.is_some() {
            resolved += 1;
        }
    }

    Ok(json!({
        "summary": match pending.len() {
            0 => "Every game already has a name.".to_string(),
            n => format!("Looked up {n} game(s), resolved {resolved}."),
        },
        "attempted": pending.len(),
        "resolved": resolved,
    }))
}

async fn vacuum(state: &AppState) -> ApiResult<Value> {
    let before = database_bytes(state).await;

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&state.pool)
        .await?;
    sqlx::query("VACUUM").execute(&state.pool).await?;

    let after = database_bytes(state).await;

    Ok(json!({
        "summary": format!(
            "Database is {} after compacting.",
            if after < before { "smaller" } else { "unchanged" }
        ),
        "beforeBytes": before,
        "afterBytes": after,
    }))
}

async fn database_bytes(state: &AppState) -> u64 {
    let db_path = state.config.database_path();
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            total += meta.len();
        }
    }
    total
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
