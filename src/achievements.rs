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

fn unlocked_at(achievement: &Value) -> i64 {
    achievement
        .get("unlockedAt")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX)
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
            json!({ "name": "FIRST_BLOOD", "unlockedAt": 100 }),
            json!({ "name": "SPEEDRUN", "unlockedAt": 300 }),
        ];
        let incoming = vec![
            json!({ "name": "FIRST_BLOOD", "unlockedAt": 50 }),
            json!({ "name": "COLLECTOR", "unlockedAt": 200 }),
        ];

        let merged = merge_achievements(existing, incoming);

        assert_eq!(merged.len(), 3);
        assert_eq!(unlocked_at(&merged[0]), 50);
        assert!(merged
            .iter()
            .any(|a| achievement_name(a) == Some("COLLECTOR")));
    }
}
