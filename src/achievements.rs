use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncAchievements {
    /// Official server's game id (`remoteId` on the launcher side).
    pub id: String,
    /// Sent by the patched launcher so achievements can be keyed by game.
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub shop: Option<String>,
    #[serde(default)]
    pub achievements: Vec<Value>,
}

fn achievement_name(achievement: &Value) -> Option<&str> {
    achievement.get("name").and_then(Value::as_str)
}

/// The launcher's `UnlockedAchievement` calls this field `unlockTime`.
/// `unlockedAt` is accepted too so anything already stored under the older
/// name keeps working.
fn unlock_time(achievement: &Value) -> Option<i64> {
    achievement
        .get("unlockTime")
        .or_else(|| achievement.get("unlockedAt"))
        .and_then(Value::as_i64)
}

/// Ordering key for "earliest unlock wins": entries with no time sort last,
/// so a real time always beats a missing one.
fn unlocked_at(achievement: &Value) -> i64 {
    unlock_time(achievement).unwrap_or(i64::MAX)
}

/// Union-merge by achievement name, keeping the earliest unlock time.
fn merge_achievements(existing: Vec<Value>, incoming: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(existing.len() + incoming.len());

    for achievement in existing.into_iter().chain(incoming) {
        let Some(name) = achievement_name(&achievement).map(str::to_string) else {
            continue;
        };

        match merged
            .iter_mut()
            .find(|entry| achievement_name(entry) == Some(name.as_str()))
        {
            Some(entry) => {
                if unlocked_at(&achievement) < unlocked_at(entry) {
                    *entry = achievement;
                }
            }
            None => merged.push(achievement),
        }
    }

    merged
}

/// PUT /profile/games/achievements
///
/// Returns the merged set as `{ objectId, shop, achievements }` when the
/// game mapping is known, otherwise 204 (the launcher falls back to its
/// local merge on an empty response).
pub async fn sync(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<SyncAchievements>,
) -> ApiResult<Response> {
    let existing = sqlx::query(
        "SELECT shop, object_id, achievements FROM game_achievements
         WHERE user_id = ? AND remote_game_id = ?",
    )
    .bind(&user.0.id)
    .bind(&payload.id)
    .fetch_optional(&state.pool)
    .await?;

    let existing_achievements: Vec<Value> = existing
        .as_ref()
        .and_then(|row| {
            serde_json::from_str(&row.get::<String, _>("achievements")).ok()
        })
        .unwrap_or_default();

    let shop = payload
        .shop
        .clone()
        .or_else(|| existing.as_ref().and_then(|row| row.get("shop")));
    let object_id = payload
        .object_id
        .clone()
        .or_else(|| existing.as_ref().and_then(|row| row.get("object_id")));

    let merged = merge_achievements(existing_achievements, payload.achievements);
    let merged_json = serde_json::to_string(&merged)
        .map_err(|_| ApiError::internal("failed to serialize achievements"))?;

    sqlx::query(
        "INSERT INTO game_achievements (user_id, remote_game_id, shop, object_id, achievements, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, remote_game_id) DO UPDATE SET
           shop = COALESCE(excluded.shop, game_achievements.shop),
           object_id = COALESCE(excluded.object_id, game_achievements.object_id),
           achievements = excluded.achievements,
           updated_at = excluded.updated_at",
    )
    .bind(&user.0.id)
    .bind(&payload.id)
    .bind(&shop)
    .bind(&object_id)
    .bind(&merged_json)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    match (object_id, shop) {
        (Some(object_id), Some(shop)) => Ok(Json(json!({
            "objectId": object_id,
            "shop": shop,
            "achievements": merged,
        }))
        .into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// GET /profile/stats/{userId} — achievement-count fallback.
///
/// The official API only computes profile achievement totals for
/// subscribers; launchers fill the gap from the achievements synced here.
/// Returns null when the user has no achievement data on this server so
/// clients don't render a misleading zero.
pub async fn user_stats(
    State(state): State<AppState>,
    _viewer: CurrentUser,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(json_array_length(achievements)), 0)
         FROM game_achievements WHERE user_id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    let sum = match row {
        Some((games, sum)) if games > 0 => Some(sum),
        _ => None,
    };

    Ok(Json(json!({ "unlockedAchievementSum": sum })))
}

/// How many games' worth of recent unlocks a profile view gets back, and how
/// many achievements are kept per game. The launcher shows far fewer than
/// this; the slack lets it drop entries whose metadata it can't resolve.
const RECENT_GAMES_LIMIT: usize = 6;
const RECENT_ACHIEVEMENTS_PER_GAME: usize = 10;

/// One game's most recent unlocks, paired with the unlock time used to rank
/// it against other games. `None` when nothing in the game is unlocked.
fn recent_game(shop: String, object_id: String, achievements: &[Value]) -> Option<(i64, Value)> {
    /* Only unlocked entries carry a time; the rest can't be ranked by
       recency and would just be noise on a profile. */
    let mut unlocked: Vec<(i64, &Value)> = achievements
        .iter()
        .filter_map(|achievement| Some((unlock_time(achievement)?, achievement)))
        .collect();

    unlocked.sort_by_key(|(time, _)| std::cmp::Reverse(*time));

    let most_recent = unlocked.first().map(|(time, _)| *time)?;

    let trimmed: Vec<Value> = unlocked
        .into_iter()
        .take(RECENT_ACHIEVEMENTS_PER_GAME)
        .map(|(time, achievement)| {
            json!({
                "name": achievement.get("name"),
                /* Named as the launcher names it, so the client reads the
                   same field it uses for its own achievements. */
                "unlockTime": time,
            })
        })
        .collect();

    Some((
        most_recent,
        json!({
            "shop": shop,
            "objectId": object_id,
            "achievements": trimmed,
        }),
    ))
}

/// GET /profile/achievements/{userId} — recently unlocked achievements.
///
/// The official API only compares achievements for subscribers, so profiles
/// of members without one show nothing there. This serves the achievements
/// synced to this server instead. Only names and unlock times live here —
/// icons and titles come from the public catalogue, which the launcher joins
/// on. Any authenticated user may read these; they're profile content.
///
/// Deliberately NOT under `/profile/games/achievements`: the launcher mirrors
/// its achievement sync to both this server and the official API, and a path
/// under that prefix would capture the official half too.
pub async fn recent(
    State(state): State<AppState>,
    _viewer: CurrentUser,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT shop, object_id, achievements FROM game_achievements
         WHERE user_id = ? AND shop IS NOT NULL AND object_id IS NOT NULL",
    )
    .bind(&user_id)
    .fetch_all(&state.pool)
    .await?;

    let mut games: Vec<(i64, Value)> = rows
        .iter()
        .filter_map(|row| {
            let achievements: Vec<Value> =
                serde_json::from_str(&row.get::<String, _>("achievements")).ok()?;

            recent_game(row.get("shop"), row.get("object_id"), &achievements)
        })
        .collect();

    games.sort_by_key(|(most_recent, _)| std::cmp::Reverse(*most_recent));

    let games: Vec<Value> = games
        .into_iter()
        .take(RECENT_GAMES_LIMIT)
        .map(|(_, game)| game)
        .collect();

    Ok(Json(json!({ "games": games })))
}

/// DELETE /profile/games/achievements/{remoteGameId} — achievement reset.
pub async fn reset(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(remote_game_id): Path<String>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM game_achievements WHERE user_id = ? AND remote_game_id = ?")
        .bind(&user.0.id)
        .bind(&remote_game_id)
        .execute(&state.pool)
        .await?;

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_earliest_unlock_and_unions_names() {
        let existing = vec![
            json!({ "name": "FIRST_BLOOD", "unlockTime": 100 }),
            json!({ "name": "SPEEDRUN", "unlockTime": 300 }),
        ];
        let incoming = vec![
            json!({ "name": "FIRST_BLOOD", "unlockTime": 50 }),
            json!({ "name": "COLLECTOR", "unlockTime": 200 }),
        ];

        let merged = merge_achievements(existing, incoming);

        assert_eq!(merged.len(), 3);
        assert_eq!(unlocked_at(&merged[0]), 50);
        assert!(merged
            .iter()
            .any(|a| achievement_name(a) == Some("COLLECTOR")));
    }

    #[test]
    fn recent_game_ranks_by_newest_unlock_and_drops_locked() {
        let achievements = vec![
            json!({ "name": "OLD", "unlockTime": 100 }),
            json!({ "name": "LOCKED" }),
            json!({ "name": "NEW", "unlockTime": 900 }),
        ];

        let (most_recent, game) =
            recent_game("steam".into(), "440".into(), &achievements).expect("game");

        assert_eq!(most_recent, 900);
        assert_eq!(game["objectId"], "440");

        let names: Vec<&str> = game["achievements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();

        assert_eq!(names, vec!["NEW", "OLD"]);
    }

    /// The launcher stores `unlockTime`. Reading the wrong field made every
    /// achievement look locked, so profiles came back empty while the
    /// database was full.
    #[test]
    fn recent_game_reads_the_launcher_unlock_field() {
        let launcher_payload = vec![json!({ "name": "FIRST_BLOOD", "unlockTime": 1700 })];

        let (most_recent, game) =
            recent_game("steam".into(), "440".into(), &launcher_payload).expect("game");

        assert_eq!(most_recent, 1700);
        assert_eq!(game["achievements"][0]["unlockTime"], 1700);
    }

    /// Rows written before the field name was corrected still resolve.
    #[test]
    fn recent_game_accepts_the_legacy_unlock_field() {
        let legacy = vec![json!({ "name": "FIRST_BLOOD", "unlockedAt": 1700 })];

        let (most_recent, _) =
            recent_game("steam".into(), "440".into(), &legacy).expect("game");

        assert_eq!(most_recent, 1700);
    }

    #[test]
    fn merge_prefers_a_real_unlock_time_over_a_missing_one() {
        let merged = merge_achievements(
            vec![json!({ "name": "FIRST_BLOOD" })],
            vec![json!({ "name": "FIRST_BLOOD", "unlockTime": 50 })],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(unlocked_at(&merged[0]), 50);
    }

    #[test]
    fn recent_game_skips_games_with_nothing_unlocked() {
        let achievements = vec![json!({ "name": "LOCKED" })];

        assert!(recent_game("steam".into(), "440".into(), &achievements).is_none());
    }
}
