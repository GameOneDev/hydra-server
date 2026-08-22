//! What a signed-in player can see and do with their own data.
//!
//! Every query here is filtered by the session's user id, and every mutation
//! re-checks ownership before touching a row — a portal user must never be
//! able to name someone else's save and have it work.

use super::PortalSession;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::{cloud_saves, storage};
use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/portal/api/overview", get(overview))
        .route("/portal/api/saves", get(saves))
        .route("/portal/api/library", get(library))
        .route("/portal/api/playtime", get(playtime))
        .route("/portal/api/cloud-saves/{id}", delete(delete_snapshot))
        .route("/portal/api/cloud-saves/{id}/files", get(snapshot_files))
        .route(
            "/portal/api/cloud-saves/{id}/files/{hash}/download",
            get(download_snapshot_file),
        )
        .route("/portal/api/souvenirs/{id}", delete(delete_souvenir))
        .route("/portal/api/backups/{id}", delete(delete_backup))
        .route("/portal/api/backups/{id}/download", get(download_backup))
        .route(
            "/portal/api/emulation-saves/{id}",
            delete(delete_emulation_save),
        )
        .route(
            "/portal/api/emulation-saves/{id}/download",
            get(download_emulation_save),
        )
}

/// GET /portal/api/overview — "what have I got here, and how much room is
/// left".
async fn overview(State(state): State<AppState>, portal: PortalSession) -> ApiResult<Json<Value>> {
    let quota = state.settings.read().await.max_bytes_per_user;
    let used = storage::used_bytes(&state, &portal.user_id).await?;

    let row = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM cloud_save_snapshots s
              WHERE s.user_id = ?1 AND s.status = 'committed') AS cloud_saves,
            (SELECT COUNT(*) FROM artifacts a WHERE a.user_id = ?1) AS backups,
            (SELECT COUNT(*) FROM emulation_saves e WHERE e.user_id = ?1) AS emulation_saves,
            (SELECT COUNT(*) FROM game_achievements g WHERE g.user_id = ?1) AS achievement_games,
            (SELECT COUNT(*) FROM game_artwork w WHERE w.user_id = ?1) AS artwork,
            (SELECT COALESCE(SUM(seconds), 0) FROM playtime_daily p WHERE p.user_id = ?1)
              AS playtime_seconds,
            (SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs b WHERE b.user_id = ?1)
              AS cloud_save_bytes,
            (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts a WHERE a.user_id = ?1)
              AS backup_bytes,
            (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves e
              WHERE e.user_id = ?1) AS emulation_bytes,
            (SELECT COALESCE(SUM(size_in_bytes), 0) FROM game_artwork w WHERE w.user_id = ?1)
              AS artwork_bytes,
            (SELECT COUNT(*) FROM souvenirs v
              WHERE v.user_id = ?1 AND v.status = 'ready' AND v.is_uploaded = 1) AS souvenirs,
            (SELECT COALESCE(SUM(size_in_bytes), 0) FROM souvenirs v WHERE v.user_id = ?1)
              AS souvenir_bytes",
    )
    .bind(&portal.user_id)
    .fetch_one(&state.pool)
    .await?;

    let devices = sqlx::query(
        "SELECT hostname, platform, MAX(at) AS last_seen_at, COUNT(*) AS items
         FROM (
             SELECT hostname, platform, updated_at AS at FROM cloud_save_snapshots
              WHERE user_id = ?1 AND hostname IS NOT NULL AND hostname <> ''
             UNION ALL
             SELECT hostname, platform, created_at FROM artifacts
              WHERE user_id = ?1 AND hostname <> ''
             UNION ALL
             SELECT hostname, platform, updated_at FROM emulation_saves
              WHERE user_id = ?1 AND hostname IS NOT NULL AND hostname <> ''
         )
         GROUP BY hostname ORDER BY last_seen_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let activity = sqlx::query(&format!(
        "SELECT {} {} WHERE e.user_id = ? AND e.category = 'sync' ORDER BY e.at DESC LIMIT 15",
        crate::events::EVENT_COLUMNS,
        crate::events::EVENT_JOINS
    ))
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "usedBytes": used,
        "quotaBytes": quota,
        "quotaRatio": if quota > 0 { used as f64 / quota as f64 } else { 0.0 },
        "counts": {
            "cloudSaves": row.get::<i64, _>("cloud_saves"),
            "backups": row.get::<i64, _>("backups"),
            "emulationSaves": row.get::<i64, _>("emulation_saves"),
            "achievementGames": row.get::<i64, _>("achievement_games"),
            "artwork": row.get::<i64, _>("artwork"),
            "souvenirs": row.get::<i64, _>("souvenirs"),
        },
        "playtimeSeconds": row.get::<i64, _>("playtime_seconds"),
        "storage": [
            { "key": "cloudSaves", "label": "Cloud saves", "bytes": row.get::<i64, _>("cloud_save_bytes") },
            { "key": "backups", "label": "Save backups", "bytes": row.get::<i64, _>("backup_bytes") },
            { "key": "emulationSaves", "label": "Emulation saves", "bytes": row.get::<i64, _>("emulation_bytes") },
            { "key": "artwork", "label": "Custom images", "bytes": row.get::<i64, _>("artwork_bytes") },
            { "key": "souvenirs", "label": "Souvenirs", "bytes": row.get::<i64, _>("souvenir_bytes") },
        ],
        "devices": devices.iter().map(|row| json!({
            "hostname": row.get::<Option<String>, _>("hostname"),
            "platform": row.get::<Option<String>, _>("platform"),
            "items": row.get::<i64, _>("items"),
            "lastSeenAt": row.get::<Option<String>, _>("last_seen_at"),
        })).collect::<Vec<_>>(),
        "activity": activity.iter().map(crate::events::row_json).collect::<Vec<_>>(),
    })))
}

/// Every save the signed-in user has here, all three kinds in one list.
const SAVES: &str = "
    SELECT 'cloud' AS kind, s.id, s.shop, s.object_id, s.total_size_in_bytes AS size_bytes,
           s.updated_at AS at, s.hostname, s.platform, s.status AS state,
           s.file_count, s.version, NULL AS label, NULL AS detail, 0 AS is_frozen
      FROM cloud_save_snapshots s WHERE s.user_id = ?1
    UNION ALL
    SELECT 'legacy', a.id, a.shop, a.object_id, a.artifact_length_in_bytes, a.created_at,
           a.hostname, a.platform,
           CASE WHEN a.is_uploaded = 1 THEN 'uploaded' ELSE 'pending' END,
           NULL, NULL, a.label, a.download_option_title, a.is_frozen
      FROM artifacts a WHERE a.user_id = ?1
    UNION ALL
    SELECT 'emulation', e.id, e.shop, e.object_id, e.artifact_length_in_bytes, e.updated_at,
           e.hostname, e.platform,
           CASE WHEN e.is_uploaded = 1 THEN 'uploaded' ELSE 'pending' END,
           NULL, NULL, COALESCE(e.label, e.file_name), e.emulator, 0
      FROM emulation_saves e WHERE e.user_id = ?1";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavesQuery {
    #[serde(default)]
    r#type: Option<String>,
}

async fn saves(
    State(state): State<AppState>,
    portal: PortalSession,
    Query(query): Query<SavesQuery>,
) -> ApiResult<Json<Value>> {
    let kind = query
        .r#type
        .filter(|kind| matches!(kind.as_str(), "cloud" | "legacy" | "emulation"));

    let rows = sqlx::query(&format!(
        "SELECT x.*, g.name AS game_name, g.cover_url AS game_cover_url
         FROM ({SAVES}) x
         LEFT JOIN game_metadata g ON g.shop = x.shop AND g.object_id = x.object_id
         WHERE (?2 IS NULL OR x.kind = ?2)
         ORDER BY x.at DESC"
    ))
    .bind(&portal.user_id)
    .bind(&kind)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!(rows
        .iter()
        .map(|row| json!({
            "kind": row.get::<String, _>("kind"),
            "id": row.get::<String, _>("id"),
            "game": crate::admin::game_ref(row),
            "sizeBytes": row.get::<i64, _>("size_bytes"),
            "at": row.get::<String, _>("at"),
            "hostname": row.get::<Option<String>, _>("hostname"),
            "platform": row.get::<Option<String>, _>("platform"),
            "state": row.get::<String, _>("state"),
            "fileCount": row.get::<Option<i64>, _>("file_count"),
            "version": row.get::<Option<i64>, _>("version"),
            "label": row.get::<Option<String>, _>("label"),
            "detail": row.get::<Option<String>, _>("detail"),
            "isFrozen": row.get::<i64, _>("is_frozen") != 0,
        }))
        .collect::<Vec<_>>())))
}

/// Achievements, artwork, souvenirs, shared backups and synced download
/// sources.
async fn library(State(state): State<AppState>, portal: PortalSession) -> ApiResult<Json<Value>> {
    let achievements = sqlx::query(
        "SELECT ga.shop, ga.object_id, ga.updated_at,
                json_array_length(ga.achievements) AS total,
                (SELECT COUNT(*) FROM json_each(ga.achievements) entry
                  WHERE json_extract(entry.value, '$.unlockTime') IS NOT NULL
                     OR json_extract(entry.value, '$.unlockedAt') IS NOT NULL) AS unlocked,
                g.name AS game_name, g.cover_url AS game_cover_url
         FROM game_achievements ga
         LEFT JOIN game_metadata g ON g.shop = ga.shop AND g.object_id = ga.object_id
         WHERE ga.user_id = ? ORDER BY ga.updated_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let artwork = sqlx::query(
        "SELECT w.*, g.name AS game_name, g.cover_url AS game_cover_url
         FROM game_artwork w
         LEFT JOIN game_metadata g ON g.shop = w.shop AND g.object_id = w.object_id
         WHERE w.user_id = ? ORDER BY w.updated_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let souvenirs = sqlx::query(
        "SELECT v.id, v.shop, v.object_id, v.image_key, v.size_in_bytes, v.visibility,
                v.captured_at, v.primary_achievement_name,
                json_array_length(v.achievement_names) AS achievement_count,
                (SELECT COUNT(*) FROM souvenir_likes l WHERE l.souvenir_id = v.id) AS likes,
                g.name AS game_name, g.cover_url AS game_cover_url
         FROM souvenirs v
         LEFT JOIN game_metadata g ON g.shop = v.shop AND g.object_id = v.object_id
         WHERE v.user_id = ? AND v.status = 'ready' AND v.is_uploaded = 1
         ORDER BY v.captured_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    /* Both directions of sharing: what this user shared out, and what other
       people shared with them (which the launcher can restore). */
    let shared_out = sqlx::query(
        "SELECT sh.recipient_user_id, sh.created_at, a.label, a.shop, a.object_id,
                a.artifact_length_in_bytes AS size_bytes,
                g.name AS game_name, g.cover_url AS game_cover_url,
                r.display_name AS other_name
         FROM artifact_shares sh
         JOIN artifacts a ON a.id = sh.artifact_id
         LEFT JOIN game_metadata g ON g.shop = a.shop AND g.object_id = a.object_id
         LEFT JOIN users r ON r.id = sh.recipient_user_id
         WHERE sh.owner_user_id = ? ORDER BY sh.created_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let shared_with_me = sqlx::query(
        "SELECT sh.owner_user_id, sh.created_at, a.label, a.shop, a.object_id,
                a.artifact_length_in_bytes AS size_bytes,
                g.name AS game_name, g.cover_url AS game_cover_url,
                o.display_name AS other_name
         FROM artifact_shares sh
         JOIN artifacts a ON a.id = sh.artifact_id
         LEFT JOIN game_metadata g ON g.shop = a.shop AND g.object_id = a.object_id
         LEFT JOIN users o ON o.id = sh.owner_user_id
         WHERE sh.recipient_user_id = ? ORDER BY sh.created_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let sources = sqlx::query(
        "SELECT * FROM download_sources WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(&portal.user_id)
    .fetch_all(&state.pool)
    .await?;

    let share_json = |row: &sqlx::sqlite::SqliteRow| {
        json!({
            "game": crate::admin::game_ref(row),
            "label": row.get::<Option<String>, _>("label"),
            "sizeBytes": row.get::<i64, _>("size_bytes"),
            "otherName": row.get::<Option<String>, _>("other_name"),
            "createdAt": row.get::<String, _>("created_at"),
        })
    };

    Ok(Json(json!({
        "achievements": achievements.iter().map(|row| json!({
            "game": crate::admin::game_ref(row),
            "total": row.get::<i64, _>("total"),
            "unlocked": row.get::<i64, _>("unlocked"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
        "artwork": artwork.iter().map(|row| json!({
            "game": crate::admin::game_ref(row),
            "kind": row.get::<String, _>("kind"),
            "source": row.get::<String, _>("source"),
            "url": row.get::<String, _>("url"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
        "souvenirs": souvenirs.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "game": crate::admin::game_ref(row),
            "url": format!(
                "{}/{}",
                state.config.public_url,
                row.get::<String, _>("image_key").trim_start_matches('/')
            ),
            "primaryAchievementName": row.get::<Option<String>, _>("primary_achievement_name"),
            "achievementCount": row.get::<i64, _>("achievement_count"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "visibility": row.get::<String, _>("visibility"),
            "capturedAt": row.get::<i64, _>("captured_at"),
            "likes": row.get::<i64, _>("likes"),
        })).collect::<Vec<_>>(),
        "sharedOut": shared_out.iter().map(share_json).collect::<Vec<_>>(),
        "sharedWithMe": shared_with_me.iter().map(share_json).collect::<Vec<_>>(),
        "downloadSources": sources.iter().map(|row| json!({
            "name": row.get::<Option<String>, _>("name"),
            "url": row.get::<String, _>("url"),
            "createdAt": row.get::<String, _>("created_at"),
        })).collect::<Vec<_>>(),
    })))
}

async fn playtime(State(state): State<AppState>, portal: PortalSession) -> ApiResult<Json<Value>> {
    let since = (chrono::Utc::now().date_naive() - chrono::Duration::days(363)).to_string();

    let rows = sqlx::query(
        "SELECT p.day, SUM(p.seconds) AS seconds,
                (SELECT COALESCE(g.name, p2.object_id) FROM playtime_daily p2
                 LEFT JOIN game_metadata g ON g.shop = p2.shop AND g.object_id = p2.object_id
                 WHERE p2.user_id = p.user_id AND p2.day = p.day
                 ORDER BY p2.seconds DESC LIMIT 1) AS top_game
         FROM playtime_daily p
         WHERE p.user_id = ? AND p.day >= ?
         GROUP BY p.day ORDER BY p.day ASC",
    )
    .bind(&portal.user_id)
    .bind(&since)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!(rows
        .iter()
        .map(|row| json!({
            "day": row.get::<String, _>("day"),
            "totalSeconds": row.get::<i64, _>("seconds"),
            "games": [{ "name": row.get::<Option<String>, _>("top_game"),
                        "seconds": row.get::<i64, _>("seconds") }],
        }))
        .collect::<Vec<_>>())))
}

// ---------------------------------------------------------------------------
// Owned-row actions
// ---------------------------------------------------------------------------

/// Confirms the row belongs to the session before anything touches it.
async fn owned(state: &AppState, table: &str, id: &str, user_id: &str) -> ApiResult<()> {
    let sql = match table {
        "cloud_save_snapshots" => "SELECT id FROM cloud_save_snapshots WHERE id = ? AND user_id = ?",
        "artifacts" => "SELECT id FROM artifacts WHERE id = ? AND user_id = ?",
        "emulation_saves" => "SELECT id FROM emulation_saves WHERE id = ? AND user_id = ?",
        _ => return Err(ApiError::internal("unknown table")),
    };

    let found: Option<String> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?;

    /* Not-found rather than forbidden: someone probing ids should not learn
       which ones exist on other accounts. */
    found.map(|_| ()).ok_or_else(|| ApiError::not_found("not found"))
}

async fn snapshot_files(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    owned(&state, "cloud_save_snapshots", &id, &portal.user_id).await?;

    let rows = sqlx::query(
        "SELECT f.*, b.hash IS NOT NULL AS stored
         FROM cloud_save_snapshot_files f
         LEFT JOIN cloud_save_blobs b ON b.user_id = ? AND b.hash = f.hash
         WHERE f.snapshot_id = ? ORDER BY f.raw_path, f.relative_path",
    )
    .bind(&portal.user_id)
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!(rows
        .iter()
        .map(|row| json!({
            "rawPath": row.get::<String, _>("raw_path"),
            "relativePath": row.get::<String, _>("relative_path"),
            "hash": row.get::<String, _>("hash"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "lastModifiedAt": row.get::<String, _>("last_modified_at"),
            "stored": row.get::<i64, _>("stored") != 0,
        }))
        .collect::<Vec<_>>())))
}

async fn download_snapshot_file(
    State(state): State<AppState>,
    portal: PortalSession,
    Path((id, hash)): Path<(String, String)>,
) -> ApiResult<Redirect> {
    owned(&state, "cloud_save_snapshots", &id, &portal.user_id).await?;

    let belongs: Option<String> = sqlx::query_scalar(
        "SELECT hash FROM cloud_save_snapshot_files WHERE snapshot_id = ? AND hash = ? LIMIT 1",
    )
    .bind(&id)
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?;
    if belongs.is_none() {
        return Err(ApiError::not_found("file not found"));
    }

    let url = storage::sign_download_url(
        &state,
        &storage::cloud_save_blob_key(&portal.user_id, &hash),
    );
    Ok(Redirect::temporary(&url))
}

async fn delete_snapshot(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    owned(&state, "cloud_save_snapshots", &id, &portal.user_id).await?;

    let before = storage::used_bytes(&state, &portal.user_id).await?;

    sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    cloud_saves::collect_orphan_blobs(&state, &portal.user_id).await?;
    let after = storage::used_bytes(&state, &portal.user_id).await?;

    crate::events::record(
        &state,
        Event::sync("cloud_save.deleted", &portal.user_id, "Deleted a cloud save from the portal")
            .detail(json!({ "snapshotId": id, "via": "portal" }))
            .size(before - after),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": before - after })))
}

async fn download_backup(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Redirect> {
    owned(&state, "artifacts", &id, &portal.user_id).await?;
    Ok(Redirect::temporary(&storage::sign_download_url(
        &state,
        &format!("artifacts/{id}.tar"),
    )))
}

async fn delete_backup(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    owned(&state, "artifacts", &id, &portal.user_id).await?;

    let size: i64 =
        sqlx::query_scalar("SELECT artifact_length_in_bytes FROM artifacts WHERE id = ?")
            .bind(&id)
            .fetch_one(&state.pool)
            .await?;

    sqlx::query("DELETE FROM artifacts WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    storage::delete_object(&state, &format!("artifacts/{id}.tar")).await;

    crate::events::record(
        &state,
        Event::sync("backup.deleted", &portal.user_id, "Deleted a save backup from the portal")
            .detail(json!({ "artifactId": id, "via": "portal" }))
            .size(size),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": size })))
}

/// DELETE /portal/api/souvenirs/{id} — a player removing their own screenshot.
///
/// The launcher can do this from the profile too; someone who captured a
/// souvenir they would rather nobody saw shouldn't have to start a game to get
/// rid of it.
async fn delete_souvenir(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let freed = crate::souvenirs::delete_owned(&state, &portal.user_id, &id).await?;

    Ok(Json(json!({ "ok": true, "freedBytes": freed })))
}

async fn download_emulation_save(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Redirect> {
    owned(&state, "emulation_saves", &id, &portal.user_id).await?;
    Ok(Redirect::temporary(&storage::sign_download_url(
        &state,
        &format!("emulation-saves/{id}.bin"),
    )))
}

async fn delete_emulation_save(
    State(state): State<AppState>,
    portal: PortalSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    owned(&state, "emulation_saves", &id, &portal.user_id).await?;

    let size: i64 =
        sqlx::query_scalar("SELECT artifact_length_in_bytes FROM emulation_saves WHERE id = ?")
            .bind(&id)
            .fetch_one(&state.pool)
            .await?;

    sqlx::query("DELETE FROM emulation_saves WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    storage::delete_object(&state, &format!("emulation-saves/{id}.bin")).await;

    crate::events::record(
        &state,
        Event::sync(
            "emulation_save.deleted",
            &portal.user_id,
            "Deleted an emulation save from the portal",
        )
        .detail(json!({ "saveId": id, "via": "portal" }))
        .size(size),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": size })))
}
