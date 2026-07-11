use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::BTreeMap;

/// The launcher reports every ~3 minutes, so a single delta anywhere near a
/// full day is a client bug (or tampering) and gets clamped.
const MAX_DELTA_SECONDS: i64 = 24 * 60 * 60;

const DEFAULT_RANGE_DAYS: i64 = 35;
const MAX_RANGE_DAYS: i64 = 366;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportPlaytime {
    pub shop: String,
    pub object_id: String,
    pub delta_in_seconds: i64,
    /// Player-local calendar date (YYYY-MM-DD); server UTC date when absent.
    #[serde(default)]
    pub day: Option<String>,
}

/// POST /profile/playtime — playtime delta reported while a game is running.
pub async fn report(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<ReportPlaytime>,
) -> ApiResult<StatusCode> {
    let shop = payload.shop.trim();
    let object_id = payload.object_id.trim();

    if shop.is_empty() || object_id.is_empty() {
        return Err(ApiError::bad_request("shop and objectId are required"));
    }

    if payload.delta_in_seconds <= 0 {
        return Ok(StatusCode::OK);
    }

    let delta = payload.delta_in_seconds.min(MAX_DELTA_SECONDS);

    let day = match payload.day.as_deref() {
        Some(day) => NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("day must be YYYY-MM-DD"))?
            .to_string(),
        None => Utc::now().date_naive().to_string(),
    };

    sqlx::query(
        "INSERT INTO playtime_daily (user_id, day, shop, object_id, seconds, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, day, shop, object_id) DO UPDATE SET
           seconds = seconds + excluded.seconds,
           updated_at = excluded.updated_at",
    )
    .bind(&user.0.id)
    .bind(&day)
    .bind(shop)
    .bind(object_id)
    .bind(delta)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct HeatmapQuery {
    #[serde(default)]
    pub days: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePlaytime {
    pub shop: String,
    pub object_id: String,
    pub seconds: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayPlaytime {
    pub day: String,
    pub total_seconds: i64,
    pub games: Vec<GamePlaytime>,
}

/// GET /profile/playtime?days=35 — per-day totals with a per-game breakdown,
/// oldest day first. Days with no playtime are omitted.
pub async fn heatmap(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<HeatmapQuery>,
) -> ApiResult<Json<Vec<DayPlaytime>>> {
    let days = query
        .days
        .unwrap_or(DEFAULT_RANGE_DAYS)
        .clamp(1, MAX_RANGE_DAYS);

    let since = (Utc::now().date_naive() - Duration::days(days - 1)).to_string();

    let rows = sqlx::query(
        "SELECT day, shop, object_id, seconds FROM playtime_daily
         WHERE user_id = ? AND day >= ?
         ORDER BY day ASC, seconds DESC",
    )
    .bind(&user.0.id)
    .bind(&since)
    .fetch_all(&state.pool)
    .await?;

    let mut by_day: BTreeMap<String, Vec<GamePlaytime>> = BTreeMap::new();

    for row in rows {
        by_day
            .entry(row.get("day"))
            .or_default()
            .push(GamePlaytime {
                shop: row.get("shop"),
                object_id: row.get("object_id"),
                seconds: row.get("seconds"),
            });
    }

    Ok(Json(
        by_day
            .into_iter()
            .map(|(day, games)| DayPlaytime {
                day,
                total_seconds: games.iter().map(|game| game.seconds).sum(),
                games,
            })
            .collect(),
    ))
}
