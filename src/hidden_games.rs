use crate::auth::CurrentUser;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenGame {
    pub id: String,
    pub shop: String,
    pub object_id: String,
    pub created_at: String,
}

/// GET /profile/hidden-games — hidden games synced across the user's devices.
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<HiddenGame>>> {
    let rows = sqlx::query(
        "SELECT id, shop, object_id, created_at FROM hidden_games
         WHERE user_id = ? ORDER BY created_at ASC",
    )
    .bind(&user.0.id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|row| HiddenGame {
                id: row.get("id"),
                shop: row.get("shop"),
                object_id: row.get("object_id"),
                created_at: row.get("created_at"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HidePayload {
    pub shop: String,
    pub object_id: String,
}

/// POST /profile/hidden-games { shop, objectId }
pub async fn hide(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<HidePayload>,
) -> ApiResult<StatusCode> {
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO hidden_games (id, user_id, shop, object_id, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(user_id, shop, object_id) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&user.0.id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    crate::events::record(
        &state,
        crate::events::Event::sync(
            "hidden_games.hidden",
            &user.0.id,
            format!("{} hid a game", user.0.display_name),
        )
        .game(&payload.shop, &payload.object_id),
    )
    .await;

    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnhideQuery {
    pub shop: String,
    pub object_id: String,
}

/// DELETE /profile/hidden-games?shop=…&objectId=…
pub async fn unhide(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<UnhideQuery>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "DELETE FROM hidden_games WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(&user.0.id)
    .bind(&query.shop)
    .bind(&query.object_id)
    .execute(&state.pool)
    .await?;

    crate::events::record(
        &state,
        crate::events::Event::sync(
            "hidden_games.unhidden",
            &user.0.id,
            format!("{} unhid a game", user.0.display_name),
        )
        .game(&query.shop, &query.object_id),
    )
    .await;

    Ok(StatusCode::OK)
}

/// The games a user has hidden, grouped by shop so membership can be tested
/// without allocating per row.
#[derive(Default)]
pub struct HiddenSet(HashMap<String, HashSet<String>>);

impl HiddenSet {
    pub fn contains(&self, shop: &str, object_id: &str) -> bool {
        self.0.get(shop).is_some_and(|ids| ids.contains(object_id))
    }
}

/// Returns the games hidden by a user.
/// Used by profile-facing endpoints to filter hidden games.
pub async fn hidden_set(pool: &SqlitePool, user_id: &str) -> Result<HiddenSet, sqlx::Error> {
    let rows = sqlx::query("SELECT shop, object_id FROM hidden_games WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(pool)
        .await?;

    let mut hidden = HiddenSet::default();
    for row in &rows {
        hidden
            .0
            .entry(row.get("shop"))
            .or_default()
            .insert(row.get("object_id"));
    }

    Ok(hidden)
}
