//! The user directory and everything an operator can do to one account.

use super::{banner_url, AdminSession, Paging};
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::{cloud_saves, storage};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/users", get(list))
        .route("/admin/api/users/{id}", get(detail).delete(delete_user))
        .route("/admin/api/users/{id}/library", get(library))
        .route("/admin/api/users/{id}/block", post(set_blocked))
        .route("/admin/api/users/{id}/purge", post(purge))
        .route("/admin/api/users/{id}/portal-link", post(portal_link))
}

/// Stored bytes for the user aliased `u`, mirroring [`storage::used_bytes`] —
/// the same sources the quota is measured against, V2 blobs counted once per
/// distinct hash exactly as they are stored. The panel and the quota must
/// never disagree about how full an account is.
pub(crate) const USED_BYTES_EXPR: &str = "
    (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts a WHERE a.user_id = u.id)
  + (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves e WHERE e.user_id = u.id)
  + (SELECT COALESCE(SUM(size_in_bytes), 0) FROM game_artwork w WHERE w.user_id = u.id)
  + (SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs b WHERE b.user_id = u.id)
  + (SELECT COALESCE(SUM(size_in_bytes), 0) FROM souvenirs v WHERE v.user_id = u.id)";

/// The counts shown for every user, in the list and on the detail screen.
const USER_COUNTS: &str = "
    (SELECT COUNT(*) FROM cloud_save_snapshots s
      WHERE s.user_id = u.id AND s.status = 'committed') AS cloud_save_count,
    (SELECT COUNT(*) FROM artifacts a WHERE a.user_id = u.id) AS backup_count,
    (SELECT COUNT(*) FROM emulation_saves e WHERE e.user_id = u.id) AS emulation_save_count,
    (SELECT COUNT(*) FROM game_achievements g WHERE g.user_id = u.id) AS achievement_game_count,
    (SELECT COUNT(*) FROM game_artwork w WHERE w.user_id = u.id AND w.size_in_bytes > 0)
      AS artwork_count,
    (SELECT COALESCE(SUM(seconds), 0) FROM playtime_daily p WHERE p.user_id = u.id)
      AS playtime_seconds,
    (SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs b WHERE b.user_id = u.id)
      AS cloud_save_bytes,
    (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts a WHERE a.user_id = u.id)
      AS backup_bytes,
    (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves e WHERE e.user_id = u.id)
      AS emulation_bytes,
    (SELECT COALESCE(SUM(size_in_bytes), 0) FROM game_artwork w WHERE w.user_id = u.id)
      AS artwork_bytes,
    (SELECT COUNT(*) FROM souvenirs v
      WHERE v.user_id = u.id AND v.status = 'ready' AND v.is_uploaded = 1) AS souvenir_count,
    (SELECT COALESCE(SUM(size_in_bytes), 0) FROM souvenirs v WHERE v.user_id = u.id)
      AS souvenir_bytes";

fn user_json(state: &AppState, row: &sqlx::sqlite::SqliteRow, quota: u64) -> Value {
    let used: i64 = row.get("used_bytes");

    json!({
        "id": row.get::<String, _>("id"),
        "username": row.get::<Option<String>, _>("username"),
        "displayName": row.get::<String, _>("display_name"),
        "profileImageUrl": row.get::<Option<String>, _>("profile_image_url"),
        "bannerUrl": banner_url(state, row.get("banner_key")),
        "isBlocked": row.get::<i64, _>("is_blocked") != 0,
        "createdAt": row.get::<String, _>("created_at"),
        "lastSeenAt": row.get::<String, _>("last_seen_at"),
        "usedBytes": used,
        "quotaBytes": quota,
        "quotaRatio": if quota > 0 { used as f64 / quota as f64 } else { 0.0 },
        "counts": {
            "cloudSaves": row.get::<i64, _>("cloud_save_count"),
            "backups": row.get::<i64, _>("backup_count"),
            "emulationSaves": row.get::<i64, _>("emulation_save_count"),
            "achievementGames": row.get::<i64, _>("achievement_game_count"),
            "artwork": row.get::<i64, _>("artwork_count"),
            "souvenirs": row.get::<i64, _>("souvenir_count"),
        },
        "playtimeSeconds": row.get::<i64, _>("playtime_seconds"),
        "storage": super::overview::storage_breakdown(
            row.get::<i64, _>("cloud_save_bytes"),
            row.get::<i64, _>("backup_bytes"),
            row.get::<i64, _>("emulation_bytes"),
            row.get::<i64, _>("artwork_bytes"),
            row.get::<i64, _>("souvenir_bytes"),
        ),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    q: Option<String>,
    /// `all` (default), `active`, `blocked`.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    per_page: Option<i64>,
}

/// GET /admin/api/users — the directory, searchable and sortable.
async fn list(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let quota = state.settings.read().await.max_bytes_per_user;
    let paging = Paging::new(query.page, query.per_page);

    let search = super::like_pattern(query.q.as_deref());
    let mut filters = vec!["1 = 1".to_string()];
    if search.is_some() {
        filters.push(
            "(u.display_name LIKE ?1 ESCAPE '\\' OR u.username LIKE ?1 ESCAPE '\\'
              OR u.id LIKE ?1 ESCAPE '\\')"
                .to_string(),
        );
    }
    /* Paging placeholders are numbered explicitly rather than left as bare
       `?`: mixing the two forms in one statement does not survive the round
       trip through the driver, and silently binds the wrong value to LIMIT. */
    let (limit_slot, offset_slot) = if search.is_some() { (2, 3) } else { (1, 2) };
    match query.status.as_deref() {
        Some("blocked") => filters.push("u.is_blocked = 1".to_string()),
        Some("active") => filters.push("u.is_blocked = 0".to_string()),
        _ => {}
    }
    let where_clause = filters.join(" AND ");

    let order = super::order_by(
        &[
            ("name", "u.display_name COLLATE NOCASE"),
            ("lastSeen", "u.last_seen_at"),
            ("created", "u.created_at"),
            ("storage", "used_bytes"),
            ("cloudSaves", "cloud_save_count"),
            ("backups", "backup_count"),
            ("playtime", "playtime_seconds"),
        ],
        query.sort.as_deref(),
        query.dir.as_deref(),
        "u.last_seen_at",
    );

    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM users u WHERE {where_clause}");
        let mut count = sqlx::query_scalar(&sql);
        if let Some(pattern) = &search {
            count = count.bind(pattern);
        }
        count.fetch_one(&state.pool).await?
    };

    let sql = format!(
        "SELECT u.*, ({USED_BYTES_EXPR}) AS used_bytes, {USER_COUNTS}
         FROM users u WHERE {where_clause}
         ORDER BY {order} LIMIT ?{limit_slot} OFFSET ?{offset_slot}"
    );
    let mut rows = sqlx::query(&sql);
    if let Some(pattern) = &search {
        rows = rows.bind(pattern);
    }
    let rows = rows
        .bind(paging.per_page())
        .bind(paging.offset())
        .fetch_all(&state.pool)
        .await?;

    let users: Vec<Value> = rows
        .iter()
        .map(|row| user_json(&state, row, quota))
        .collect();

    Ok(Json(paging.envelope(users, total)))
}

/// GET /admin/api/users/{id} — the account, its footprint, and the machines
/// it syncs from.
async fn detail(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let quota = state.settings.read().await.max_bytes_per_user;

    let row = sqlx::query(&format!(
        "SELECT u.*, ({USED_BYTES_EXPR}) AS used_bytes, {USER_COUNTS}
         FROM users u WHERE u.id = ?"
    ))
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("user not found"))?;

    /* Devices: the launcher stamps its hostname on everything it uploads, so
       the union of those is the machine list for the account — the fastest
       way to tell "synced from two PCs" from "someone else has the token". */
    let devices = sqlx::query(
        "SELECT hostname, platform, COUNT(*) AS items, MAX(at) AS last_seen_at
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
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let games = sqlx::query(
        "SELECT t.shop, t.object_id, g.name AS game_name, g.cover_url AS game_cover_url,
                SUM(t.bytes) AS bytes, MAX(t.at) AS last_at,
                COALESCE((SELECT SUM(p.seconds) FROM playtime_daily p
                          WHERE p.user_id = ?1 AND p.shop = t.shop
                            AND p.object_id = t.object_id), 0) AS seconds
         FROM (
             SELECT shop, object_id, total_size_in_bytes AS bytes, updated_at AS at
               FROM cloud_save_snapshots WHERE user_id = ?1 AND status = 'committed'
             UNION ALL
             SELECT shop, object_id, artifact_length_in_bytes, created_at
               FROM artifacts WHERE user_id = ?1
             UNION ALL
             SELECT shop, object_id, 0, updated_at FROM playtime_daily WHERE user_id = ?1
         ) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         GROUP BY t.shop, t.object_id ORDER BY bytes DESC, seconds DESC LIMIT 12",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "user": user_json(&state, &row, quota),
        "devices": devices.iter().map(|row| json!({
            "hostname": row.get::<Option<String>, _>("hostname"),
            "platform": row.get::<Option<String>, _>("platform"),
            "items": row.get::<i64, _>("items"),
            "lastSeenAt": row.get::<Option<String>, _>("last_seen_at"),
        })).collect::<Vec<_>>(),
        "games": games.iter().map(|row| json!({
            "game": super::game_ref(row),
            "bytes": row.get::<i64, _>("bytes"),
            "playtimeSeconds": row.get::<i64, _>("seconds"),
            "lastAt": row.get::<Option<String>, _>("last_at"),
        })).collect::<Vec<_>>(),
    })))
}

/// GET /admin/api/users/{id}/library — the small per-user collections that
/// aren't worth paginating: achievements, custom images, souvenirs, shares,
/// sources.
async fn library(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let achievements = sqlx::query(
        "SELECT ga.remote_game_id, ga.shop, ga.object_id, ga.updated_at,
                json_array_length(ga.achievements) AS total,
                (SELECT COUNT(*) FROM json_each(ga.achievements) entry
                  WHERE json_extract(entry.value, '$.unlockTime') IS NOT NULL
                     OR json_extract(entry.value, '$.unlockedAt') IS NOT NULL) AS unlocked,
                g.name AS game_name, g.cover_url AS game_cover_url
         FROM game_achievements ga
         LEFT JOIN game_metadata g ON g.shop = ga.shop AND g.object_id = ga.object_id
         WHERE ga.user_id = ? ORDER BY ga.updated_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let artwork = sqlx::query(
        "SELECT w.*, g.name AS game_name, g.cover_url AS game_cover_url
         FROM game_artwork w
         LEFT JOIN game_metadata g ON g.shop = w.shop AND g.object_id = w.object_id
         WHERE w.user_id = ? ORDER BY w.updated_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let souvenirs = sqlx::query(
        "SELECT v.id, v.shop, v.object_id, v.image_key, v.size_in_bytes, v.visibility,
                v.captured_at, v.primary_achievement_name,
                json_array_length(v.achievement_names) AS achievement_count,
                (SELECT COUNT(*) FROM souvenir_likes l WHERE l.souvenir_id = v.id) AS likes,
                (SELECT COUNT(*) FROM souvenir_reports r WHERE r.souvenir_id = v.id) AS reports,
                g.name AS game_name, g.cover_url AS game_cover_url
         FROM souvenirs v
         LEFT JOIN game_metadata g ON g.shop = v.shop AND g.object_id = v.object_id
         WHERE v.user_id = ? AND v.status = 'ready' AND v.is_uploaded = 1
         ORDER BY v.captured_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let shares = sqlx::query(
        "SELECT sh.*, a.label, a.shop, a.object_id, a.artifact_length_in_bytes AS size_bytes,
                g.name AS game_name, g.cover_url AS game_cover_url,
                r.display_name AS recipient_name
         FROM artifact_shares sh
         JOIN artifacts a ON a.id = sh.artifact_id
         LEFT JOIN game_metadata g ON g.shop = a.shop AND g.object_id = a.object_id
         LEFT JOIN users r ON r.id = sh.recipient_user_id
         WHERE sh.owner_user_id = ? ORDER BY sh.created_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let sources = sqlx::query(
        "SELECT * FROM download_sources WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "achievements": achievements.iter().map(|row| json!({
            "remoteGameId": row.get::<String, _>("remote_game_id"),
            "game": super::game_ref(row),
            "total": row.get::<i64, _>("total"),
            "unlocked": row.get::<i64, _>("unlocked"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
        "artwork": artwork.iter().map(|row| json!({
            "game": super::game_ref(row),
            "kind": row.get::<String, _>("kind"),
            "source": row.get::<String, _>("source"),
            "url": row.get::<String, _>("url"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
        "souvenirs": souvenirs.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "game": super::game_ref(row),
            /* Same public URL the profile renders, so an operator handling a
               report can look at the picture instead of guessing from a name. */
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
            "reports": row.get::<i64, _>("reports"),
        })).collect::<Vec<_>>(),
        "shares": shares.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "artifactId": row.get::<String, _>("artifact_id"),
            "recipientUserId": row.get::<String, _>("recipient_user_id"),
            "recipientName": row.get::<Option<String>, _>("recipient_name"),
            "label": row.get::<Option<String>, _>("label"),
            "sizeBytes": row.get::<i64, _>("size_bytes"),
            "game": super::game_ref(row),
            "createdAt": row.get::<String, _>("created_at"),
        })).collect::<Vec<_>>(),
        "downloadSources": sources.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "name": row.get::<Option<String>, _>("name"),
            "url": row.get::<String, _>("url"),
            "createdAt": row.get::<String, _>("created_at"),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct BlockRequest {
    blocked: bool,
}

async fn set_blocked(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(payload): Json<BlockRequest>,
) -> ApiResult<Json<Value>> {
    let result = sqlx::query("UPDATE users SET is_blocked = ? WHERE id = ?")
        .bind(payload.blocked as i64)
        .bind(&id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("user not found"));
    }

    /* Blocked users may still have a cached token — drop the cache so the
       block applies within seconds, not minutes. */
    state.token_cache.write().await.clear();

    crate::events::record(
        &state,
        Event::admin(
            if payload.blocked { "admin.user.blocked" } else { "admin.user.unblocked" },
            if payload.blocked { "Blocked a user" } else { "Unblocked a user" },
        )
        .about(&id)
        .warning(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "isBlocked": payload.blocked })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PurgeRequest {
    /// Any of: cloudSaves, backups, emulationSaves, artwork, achievements,
    /// playtime, downloadSources, shares.
    categories: Vec<String>,
}

/// POST /admin/api/users/{id}/purge — delete some of a user's data without
/// deleting the account.
///
/// Deleting everything is a blunt instrument: the usual real request is "drop
/// this one user's cloud saves, they're eating the quota" or "clear their
/// achievements, the sync is corrupt". Freed bytes come back so the panel can
/// say how much space the operation actually recovered.
async fn purge(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(payload): Json<PurgeRequest>,
) -> ApiResult<Json<Value>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("user not found"));
    }

    let before = storage::used_bytes(&state, &id).await?;
    let mut purged: Vec<&str> = Vec::new();

    for category in payload.categories.iter().map(String::as_str) {
        match category {
            "cloudSaves" => {
                purge_cloud_saves(&state, &id).await?;
                purged.push("cloudSaves");
            }
            "backups" => {
                purge_backups(&state, &id).await?;
                purged.push("backups");
            }
            "emulationSaves" => {
                purge_emulation_saves(&state, &id).await?;
                purged.push("emulationSaves");
            }
            "artwork" => {
                purge_artwork(&state, &id).await?;
                purged.push("artwork");
            }
            "souvenirs" => {
                crate::souvenirs::purge_for_user(&state, &id).await?;
                purged.push("souvenirs");
            }
            "achievements" => {
                sqlx::query("DELETE FROM game_achievements WHERE user_id = ?")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                purged.push("achievements");
            }
            "playtime" => {
                sqlx::query("DELETE FROM playtime_daily WHERE user_id = ?")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                purged.push("playtime");
            }
            "downloadSources" => {
                sqlx::query("DELETE FROM download_sources WHERE user_id = ?")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                purged.push("downloadSources");
            }
            "shares" => {
                sqlx::query("DELETE FROM artifact_shares WHERE owner_user_id = ?")
                    .bind(&id)
                    .execute(&state.pool)
                    .await?;
                purged.push("shares");
            }
            other => {
                return Err(ApiError::bad_request(format!(
                    "unknown purge category: {other}"
                )))
            }
        }
    }

    let after = storage::used_bytes(&state, &id).await?;
    tracing::info!(
        "admin: purged {} for {id} ({} bytes freed)",
        purged.join(", "),
        before - after
    );

    crate::events::record(
        &state,
        Event::admin(
            "admin.user.purged",
            format!("Purged {} for a user", purged.join(", ")),
        )
        .about(&id)
        .detail(json!({ "categories": purged }))
        .size(before - after)
        .warning(),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "purged": purged,
        "freedBytes": before - after,
        "usedBytes": after,
    })))
}

async fn purge_cloud_saves(state: &AppState, user_id: &str) -> ApiResult<()> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM cloud_save_snapshots WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&state.pool)
            .await?;

    for snapshot_id in &ids {
        sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
            .bind(snapshot_id)
            .execute(&state.pool)
            .await?;
    }
    sqlx::query("DELETE FROM cloud_save_snapshots WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    /* With no manifest left, every blob is an orphan — this both deletes the
       bytes and keeps the quota honest. */
    cloud_saves::collect_orphan_blobs(state, user_id).await
}

async fn purge_backups(state: &AppState, user_id: &str) -> ApiResult<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM artifacts WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(&state.pool)
        .await?;

    sqlx::query("DELETE FROM artifacts WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    for artifact_id in &ids {
        storage::delete_object(state, &format!("artifacts/{artifact_id}.tar")).await;
    }

    Ok(())
}

async fn purge_emulation_saves(state: &AppState, user_id: &str) -> ApiResult<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM emulation_saves WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(&state.pool)
        .await?;

    sqlx::query("DELETE FROM emulation_saves WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    for save_id in &ids {
        storage::delete_object(state, &format!("emulation-saves/{save_id}.bin")).await;
    }

    Ok(())
}

async fn purge_artwork(state: &AppState, user_id: &str) -> ApiResult<()> {
    let keys = crate::artwork::storage_keys_for_user(state, user_id).await;

    sqlx::query("DELETE FROM game_artwork WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    for key in keys {
        storage::delete_object(state, &key).await;
    }

    Ok(())
}

/// POST /admin/api/users/{id}/portal-link — a short-lived URL that signs this
/// user in to the portal.
///
/// The escape hatch for the person who can't sign in the normal way: a
/// deployment whose official API has no password endpoint, or someone who
/// simply can't get in. The link expires in minutes and grants nothing beyond
/// that one account's own data.
async fn portal_link(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let user: Option<(String, String)> =
        sqlx::query_as("SELECT id, display_name FROM users WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;
    let (id, display_name) = user.ok_or_else(|| ApiError::not_found("user not found"))?;

    let (token, ttl) = crate::portal::issue_link_token(&state, &id)?;

    crate::events::record(
        &state,
        Event::admin(
            "admin.user.portal_link",
            format!("Created a portal sign-in link for {display_name}"),
        )
        .about(&id)
        .detail(json!({ "expiresInSeconds": ttl })),
    )
    .await;

    Ok(Json(json!({
        "url": format!("{}/portal/auth/{token}", state.config.public_url),
        "expiresInSeconds": ttl,
    })))
}

/// DELETE /admin/api/users/{id} — the account and everything it owns.
async fn delete_user(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("user not found"));
    }

    let freed = storage::used_bytes(&state, &id).await?;

    /* Every stored key has to be read before the row goes: the database
       cascades, disk does not, and an id nothing points at is unrecoverable. */
    let artifact_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM artifacts WHERE user_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;
    let save_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM emulation_saves WHERE user_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;
    let blob_hashes: Vec<String> =
        sqlx::query_scalar("SELECT hash FROM cloud_save_blobs WHERE user_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;
    let banner_key: Option<String> = sqlx::query_scalar("SELECT banner_key FROM users WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?
        .flatten();
    let artwork_keys = crate::artwork::storage_keys_for_user(&state, &id).await;
    let souvenir_keys = crate::souvenirs::storage_keys_for_user(&state, &id).await;

    /* Likes this user left on other people's souvenirs aren't reachable from
       their own rows, so the cascade below doesn't take them. */
    sqlx::query("DELETE FROM souvenir_likes WHERE user_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    for key in artwork_keys {
        storage::delete_object(&state, &key).await;
    }
    for key in souvenir_keys {
        storage::delete_object(&state, &key).await;
    }
    if let Some(key) = banner_key {
        storage::delete_object(&state, &key).await;
    }
    for artifact_id in artifact_ids {
        storage::delete_object(&state, &format!("artifacts/{artifact_id}.tar")).await;
    }
    for save_id in save_ids {
        storage::delete_object(&state, &format!("emulation-saves/{save_id}.bin")).await;
    }
    if !blob_hashes.is_empty() {
        for hash in &blob_hashes {
            storage::delete_object(&state, &storage::cloud_save_blob_key(&id, hash)).await;
        }
        /* Succeeds only once the user's blob directory is empty, which is
           exactly when it should go. */
        let _ = tokio::fs::remove_dir(storage::storage_path(
            &state,
            &format!("cloud-saves/{id}"),
        ))
        .await;
    }

    state.token_cache.write().await.clear();
    tracing::info!("admin: deleted user {id} ({freed} bytes freed)");

    crate::events::record(
        &state,
        Event::admin("admin.user.deleted", format!("Deleted the account {id}"))
            .about(&id)
            .size(freed)
            .critical(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": freed })))
}
