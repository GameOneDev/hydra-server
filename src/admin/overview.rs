//! The dashboard: what this server holds, what changed lately, and anything
//! that needs an operator's attention.

use super::AdminSession;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/trends", get(trends))
        .route("/admin/api/playtime", get(playtime_heatmap))
}

/// A save that hasn't finished uploading after this long is stuck, not slow.
const STALE_UPLOAD_HOURS: i64 = 24;

async fn scalar(state: &AppState, sql: &str) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(sql).fetch_one(&state.pool).await?)
}

/// GET /admin/api/overview — every headline number on one screen.
///
/// Deliberately one round trip: these are cheap aggregates over a small
/// database, and a dashboard that paints in six waves feels broken.
async fn overview(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let now = Utc::now();
    let day_7 = (now - Duration::days(7)).to_rfc3339();
    let day_30 = (now - Duration::days(30)).to_rfc3339();
    let stale_before = (now - Duration::hours(STALE_UPLOAD_HOURS)).to_rfc3339();

    let users: i64 = scalar(&state, "SELECT COUNT(*) FROM users").await?;
    let blocked: i64 = scalar(&state, "SELECT COUNT(*) FROM users WHERE is_blocked = 1").await?;
    let active_7d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE last_seen_at >= ?")
        .bind(&day_7)
        .fetch_one(&state.pool)
        .await?;
    let active_30d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE last_seen_at >= ?")
        .bind(&day_30)
        .fetch_one(&state.pool)
        .await?;
    let new_7d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE created_at >= ?")
        .bind(&day_7)
        .fetch_one(&state.pool)
        .await?;

    let cloud_saves: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM cloud_save_snapshots WHERE status = 'committed'",
    )
    .await?;
    let cloud_files: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(file_count), 0) FROM cloud_save_snapshots WHERE status = 'committed'",
    )
    .await?;
    let cloud_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs",
    )
    .await?;
    let cloud_logical_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(total_size_in_bytes), 0) FROM cloud_save_snapshots
         WHERE status = 'committed'",
    )
    .await?;
    let pending_snapshots: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM cloud_save_snapshots WHERE status = 'pending'",
    )
    .await?;
    let stuck_snapshots: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cloud_save_snapshots WHERE status = 'pending' AND created_at < ?",
    )
    .bind(&stale_before)
    .fetch_one(&state.pool)
    .await?;

    let backups: i64 = scalar(&state, "SELECT COUNT(*) FROM artifacts").await?;
    let backup_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts",
    )
    .await?;
    let frozen_backups: i64 = scalar(&state, "SELECT COUNT(*) FROM artifacts WHERE is_frozen = 1")
        .await?;
    let shares: i64 = scalar(&state, "SELECT COUNT(*) FROM artifact_shares").await?;
    let stuck_backups: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifacts WHERE is_uploaded = 0 AND created_at < ?",
    )
    .bind(&stale_before)
    .fetch_one(&state.pool)
    .await?;

    let emulation_saves: i64 = scalar(&state, "SELECT COUNT(*) FROM emulation_saves").await?;
    let emulation_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves",
    )
    .await?;

    let artwork: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM game_artwork WHERE size_in_bytes > 0",
    )
    .await?;
    let artwork_linked: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM game_artwork WHERE size_in_bytes = 0",
    )
    .await?;
    let artwork_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(size_in_bytes), 0) FROM game_artwork",
    )
    .await?;

    let souvenirs: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM souvenirs WHERE status = 'ready' AND is_uploaded = 1",
    )
    .await?;
    let souvenir_bytes: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(size_in_bytes), 0) FROM souvenirs",
    )
    .await?;
    /* Reservations whose upload never arrived. Sweepable from Maintenance,
       and worth surfacing next to the other stuck-upload counts. */
    let stuck_souvenirs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM souvenirs WHERE status = 'pending' AND created_at < ?",
    )
    .bind(&stale_before)
    .fetch_one(&state.pool)
    .await?;
    let souvenir_reports: i64 = scalar(&state, "SELECT COUNT(*) FROM souvenir_reports").await?;

    let achievement_games: i64 = scalar(&state, "SELECT COUNT(*) FROM game_achievements").await?;
    let achievements_unlocked: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(
            (SELECT COUNT(*) FROM json_each(ga.achievements) entry
             WHERE json_extract(entry.value, '$.unlockTime') IS NOT NULL
                OR json_extract(entry.value, '$.unlockedAt') IS NOT NULL)
         ), 0) FROM game_achievements ga",
    )
    .await?;

    let download_sources: i64 = scalar(&state, "SELECT COUNT(*) FROM download_sources").await?;
    let playtime_seconds: i64 = scalar(
        &state,
        "SELECT COALESCE(SUM(seconds), 0) FROM playtime_daily",
    )
    .await?;

    /* A game counts once no matter how many ways it shows up here. */
    let games: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM (
             SELECT shop, object_id FROM cloud_save_snapshots
             UNION SELECT shop, object_id FROM artifacts
             UNION SELECT shop, object_id FROM playtime_daily
             UNION SELECT shop, object_id FROM game_artwork
             UNION SELECT shop, object_id FROM game_achievements WHERE shop IS NOT NULL
             UNION SELECT shop, object_id FROM souvenirs WHERE shop IS NOT NULL
         )",
    )
    .await?;

    /* Blobs the database expects but disk may not have: reported as a count
       only — the storage screen does the expensive per-file verification. */
    let missing_blobs: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM (
             SELECT DISTINCT f.hash, s.user_id
             FROM cloud_save_snapshot_files f
             JOIN cloud_save_snapshots s ON s.id = f.snapshot_id
             WHERE s.status = 'committed'
               AND NOT EXISTS (
                 SELECT 1 FROM cloud_save_blobs b
                 WHERE b.user_id = s.user_id AND b.hash = f.hash
               )
         )",
    )
    .await?;

    /* WAL/SHM files hold data not yet checkpointed into the main file, so
       count them into the database size too. */
    let db_path = state.config.database_path();
    let mut database_bytes: u64 = 0;
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            database_bytes += meta.len();
        }
    }

    let current = state.settings.read().await.clone();

    let over_quota: i64 = if current.max_bytes_per_user > 0 {
        sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM users u WHERE ({}) >= ?",
            super::users::USED_BYTES_EXPR
        ))
        .bind(current.max_bytes_per_user as i64)
        .fetch_one(&state.pool)
        .await?
    } else {
        0
    };

    let stored_bytes = cloud_bytes + backup_bytes + emulation_bytes + artwork_bytes;

    /* Alerts are the panel's reason to be checked at all: each is a condition
       an operator would want to act on, with the screen that acts on it. */
    let mut alerts: Vec<Value> = Vec::new();
    if stuck_snapshots > 0 {
        alerts.push(json!({
            "level": "warning",
            "title": format!("{stuck_snapshots} cloud save upload(s) never finished"),
            "detail": format!("Pending for more than {STALE_UPLOAD_HOURS}h. They hold no data the launcher can restore and are swept automatically on the owner's next upload."),
            "action": { "label": "Run the sweep", "route": "#/maintenance" },
        }));
    }
    if stuck_backups > 0 {
        alerts.push(json!({
            "level": "warning",
            "title": format!("{stuck_backups} legacy backup(s) stuck mid-upload"),
            "detail": "Created more than a day ago and still not uploaded — the launcher likely died mid-transfer.",
            "action": { "label": "Review backups", "route": "#/saves?type=legacy&state=pending" },
        }));
    }
    if stuck_souvenirs > 0 {
        alerts.push(json!({
            "level": "warning",
            "title": format!("{stuck_souvenirs} souvenir upload(s) never finished"),
            "detail": format!("Reserved more than {STALE_UPLOAD_HOURS}h ago and never claimed by an achievement sync. They show on nobody's profile; the sweep reclaims their space."),
            "action": { "label": "Run the sweep", "route": "#/maintenance" },
        }));
    }
    if souvenir_reports > 0 {
        alerts.push(json!({
            "level": "warning",
            "title": format!("{souvenir_reports} souvenir(s) reported by players"),
            "detail": "Someone flagged a screenshot on a profile. The report says who, why and which picture.",
            "action": { "label": "Read the reports", "route": "#/events?kind=souvenir.reported" },
        }));
    }
    if missing_blobs > 0 {
        alerts.push(json!({
            "level": "critical",
            "title": format!("{missing_blobs} cloud save file(s) have no stored bytes"),
            "detail": "A committed snapshot references content this server can't produce, so a restore would come back incomplete.",
            "action": { "label": "Check integrity", "route": "#/storage" },
        }));
    }
    if over_quota > 0 {
        alerts.push(json!({
            "level": "warning",
            "title": format!("{over_quota} user(s) at their storage quota"),
            "detail": "Their next upload is refused until they free space or the quota is raised.",
            "action": { "label": "Review users", "route": "#/users?sort=storage" },
        }));
    }
    if blocked > 0 {
        alerts.push(json!({
            "level": "info",
            "title": format!("{blocked} blocked user(s)"),
            "detail": "Blocked users keep their data but can't sync.",
            "action": { "label": "Review users", "route": "#/users?status=blocked" },
        }));
    }

    Ok(Json(json!({
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "uptimeSeconds": (now - state.started_at).num_seconds(),
            "startedAt": state.started_at.to_rfc3339(),
            "publicUrl": state.config.public_url,
            "officialApiUrl": state.config.official_api_url,
            "databaseBytes": database_bytes,
            "storedBytes": stored_bytes,
        },
        "settings": {
            "maxBytesPerUser": current.max_bytes_per_user,
            "backupsPerGameLimit": current.backups_per_game_limit,
            "allowedUsers": current.allowed_users,
        },
        "users": {
            "total": users,
            "blocked": blocked,
            "active7d": active_7d,
            "active30d": active_30d,
            "new7d": new_7d,
            "overQuota": over_quota,
        },
        "cloudSaves": {
            "committed": cloud_saves,
            "pending": pending_snapshots,
            "stuck": stuck_snapshots,
            "files": cloud_files,
            "bytes": cloud_bytes,
            "logicalBytes": cloud_logical_bytes,
            "missingBlobs": missing_blobs,
        },
        "backups": {
            "total": backups,
            "frozen": frozen_backups,
            "shared": shares,
            "stuck": stuck_backups,
            "bytes": backup_bytes,
        },
        "emulationSaves": { "total": emulation_saves, "bytes": emulation_bytes },
        "artwork": { "uploaded": artwork, "linked": artwork_linked, "bytes": artwork_bytes },
        "achievements": { "games": achievement_games, "unlocked": achievements_unlocked },
        "souvenirs": {
            "total": souvenirs,
            "stuck": stuck_souvenirs,
            "reports": souvenir_reports,
            "bytes": souvenir_bytes,
        },
        "library": { "games": games, "playtimeSeconds": playtime_seconds, "downloadSources": download_sources },
        "storage": storage_breakdown(
            cloud_bytes,
            backup_bytes,
            emulation_bytes,
            artwork_bytes,
            souvenir_bytes,
        ),
        "alerts": alerts,
    })))
}

/// The one storage split the whole panel uses, in the fixed order the chart
/// palette assigns colours by. Categories never reorder by size — a category
/// keeps its colour when it shrinks to nothing.
pub(crate) fn storage_breakdown(
    cloud: i64,
    backups: i64,
    emulation: i64,
    artwork: i64,
    souvenirs: i64,
) -> Vec<Value> {
    vec![
        json!({ "key": "cloudSaves", "label": "Cloud saves (v2)", "bytes": cloud }),
        json!({ "key": "backups", "label": "Save backups", "bytes": backups }),
        json!({ "key": "emulationSaves", "label": "Emulation saves", "bytes": emulation }),
        json!({ "key": "artwork", "label": "Custom images", "bytes": artwork }),
        json!({ "key": "souvenirs", "label": "Souvenirs", "bytes": souvenirs }),
    ]
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrendsQuery {
    #[serde(default)]
    days: Option<i64>,
}

/// GET /admin/api/trends — daily series behind the dashboard charts, plus the
/// leaderboards that make "what is this server actually storing" answerable.
async fn trends(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<TrendsQuery>,
) -> ApiResult<Json<Value>> {
    let days = query.days.unwrap_or(30).clamp(7, 180);
    let since = (Utc::now() - Duration::days(days - 1))
        .date_naive()
        .to_string();

    /* substr(at, 1, 10) turns an RFC3339 timestamp into its calendar day
       without pulling every row into memory to parse it. */
    let rows = sqlx::query(
        "SELECT substr(at, 1, 10) AS day, category, COUNT(*) AS events,
                COALESCE(SUM(size_bytes), 0) AS bytes
         FROM events
         WHERE substr(at, 1, 10) >= ?
         GROUP BY day, category ORDER BY day ASC",
    )
    .bind(&since)
    .fetch_all(&state.pool)
    .await?;

    let mut by_day: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
        Default::default();
    for row in &rows {
        let day: String = row.get("day");
        let category: String = row.get("category");
        let entry = by_day.entry(day).or_default();
        entry.insert(category, json!(row.get::<i64, _>("events")));
        let bytes: i64 = row.get("bytes");
        let total = entry
            .get("bytes")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        entry.insert("bytes".into(), json!(total + bytes));
    }

    let series: Vec<Value> = by_day
        .into_iter()
        .map(|(day, counts)| {
            let mut entry = counts;
            entry.insert("day".into(), json!(day));
            Value::Object(entry)
        })
        .collect();

    let top_users = sqlx::query(&format!(
        "SELECT u.id AS user_id, u.display_name, u.username, u.profile_image_url,
                ({}) AS bytes
         FROM users u ORDER BY bytes DESC LIMIT 5",
        super::users::USED_BYTES_EXPR
    ))
    .fetch_all(&state.pool)
    .await?;

    let top_games = sqlx::query(
        "SELECT t.shop, t.object_id, g.name AS game_name, g.cover_url AS game_cover_url,
                SUM(t.bytes) AS bytes, COUNT(DISTINCT t.user_id) AS players,
                COALESCE((SELECT SUM(p.seconds) FROM playtime_daily p
                          WHERE p.shop = t.shop AND p.object_id = t.object_id), 0) AS seconds
         FROM (
             SELECT user_id, shop, object_id, total_size_in_bytes AS bytes
               FROM cloud_save_snapshots WHERE status = 'committed'
             UNION ALL
             SELECT user_id, shop, object_id, artifact_length_in_bytes FROM artifacts
         ) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         GROUP BY t.shop, t.object_id ORDER BY bytes DESC LIMIT 5",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "days": days,
        "series": series,
        "topUsers": top_users.iter().map(|row| json!({
            "user": super::user_ref(row),
            "bytes": row.get::<i64, _>("bytes"),
        })).collect::<Vec<_>>(),
        "topGames": top_games.iter().map(|row| json!({
            "game": super::game_ref(row),
            "bytes": row.get::<i64, _>("bytes"),
            "players": row.get::<i64, _>("players"),
            "playtimeSeconds": row.get::<i64, _>("seconds"),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaytimeQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    user_id: Option<String>,
}

/// Days with playtime keep only their biggest games in the payload; the
/// tooltip never shows more than a few anyway.
const HEATMAP_GAMES_PER_DAY: usize = 3;

/// GET /admin/api/playtime?days=364[&userId=…] — daily playtime buckets,
/// aggregated across every user unless a userId is given.
async fn playtime_heatmap(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<PlaytimeQuery>,
) -> ApiResult<Json<Value>> {
    let days = query.days.unwrap_or(364).clamp(1, 366);
    let since = (Utc::now().date_naive() - Duration::days(days - 1)).to_string();

    let mut sql = String::from(
        "SELECT p.day, p.shop, p.object_id, SUM(p.seconds) AS seconds,
            COUNT(DISTINCT p.user_id) AS player_count, g.name AS game_name
         FROM playtime_daily p
         LEFT JOIN game_metadata g ON g.shop = p.shop AND g.object_id = p.object_id
         WHERE p.day >= ?",
    );
    if query.user_id.is_some() {
        sql.push_str(" AND p.user_id = ?");
    }
    sql.push_str(" GROUP BY p.day, p.shop, p.object_id ORDER BY p.day ASC, seconds DESC");

    let mut db_query = sqlx::query(&sql).bind(&since);
    if let Some(user_id) = &query.user_id {
        db_query = db_query.bind(user_id);
    }
    let rows = db_query.fetch_all(&state.pool).await?;

    /* Distinct players per day can't be derived from the per-game grouping
       above (one player may appear under several games). */
    let mut players_by_day: std::collections::BTreeMap<String, i64> = Default::default();
    if query.user_id.is_none() {
        let player_rows = sqlx::query(
            "SELECT day, COUNT(DISTINCT user_id) AS player_count
             FROM playtime_daily WHERE day >= ? GROUP BY day",
        )
        .bind(&since)
        .fetch_all(&state.pool)
        .await?;

        for row in player_rows {
            players_by_day.insert(row.get("day"), row.get("player_count"));
        }
    }

    /* Totals count every game; the games list keeps only the biggest ones
       (rows arrive seconds DESC within each day). */
    let mut by_day: std::collections::BTreeMap<String, (i64, Vec<Value>)> = Default::default();
    for row in rows {
        let day: String = row.get("day");
        let seconds: i64 = row.get("seconds");
        let (total, games) = by_day.entry(day).or_default();
        *total += seconds;
        if games.len() < HEATMAP_GAMES_PER_DAY {
            games.push(json!({
                "shop": row.get::<String, _>("shop"),
                "objectId": row.get::<String, _>("object_id"),
                "name": row.get::<Option<String>, _>("game_name"),
                "seconds": seconds,
            }));
        }
    }

    Ok(Json(json!(by_day
        .into_iter()
        .map(|(day, (total, games))| {
            json!({
                "day": day.clone(),
                "totalSeconds": total,
                "playerCount": players_by_day.get(&day).copied().unwrap_or(1),
                "games": games,
            })
        })
        .collect::<Vec<_>>())))
}
