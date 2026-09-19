//! The same data as [`super::saves`], pivoted by game.
//!
//! "Which games is this server actually storing, and for whom" is a different
//! question from "what did this user upload", and answering it by user forces
//! the operator to do the join in their head.

use super::{AdminSession, Paging};
use crate::error::ApiResult;
use crate::games as metadata;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/games", get(list))
        .route("/admin/api/games/{shop}/{object_id}", get(detail))
        .route("/admin/api/games/{shop}/{object_id}/refresh", post(refresh))
}

/// Every game this server knows anything about, with what it knows.
///
/// A game earns a row from any kind of stored data — a save, a backup,
/// playtime, artwork, achievements — so nothing is invisible just because it
/// arrived through a less common route.
const GAME_UNION: &str = "
    SELECT user_id, shop, object_id, total_size_in_bytes AS bytes, 1 AS cloud_saves,
           0 AS backups, 0 AS seconds, updated_at AS at
      FROM cloud_save_snapshots WHERE status = 'committed'
    UNION ALL
    SELECT user_id, shop, object_id, artifact_length_in_bytes, 0, 1, 0, created_at
      FROM artifacts
    UNION ALL
    SELECT user_id, shop, object_id, 0, 0, 0, seconds, updated_at FROM playtime_daily
    UNION ALL
    SELECT user_id, shop, object_id, size_in_bytes, 0, 0, 0, updated_at FROM game_artwork
    UNION ALL
    SELECT user_id, shop, object_id, 0, 0, 0, 0, updated_at
      FROM game_achievements WHERE shop IS NOT NULL AND object_id IS NOT NULL
";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    shop: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    per_page: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let paging = Paging::new(query.page, query.per_page);

    let mut filters: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(pattern) = super::like_pattern(query.q.as_deref()) {
        binds.push(pattern);
        filters.push(format!(
            "(g.name LIKE ?{i} ESCAPE '\\' OR t.object_id LIKE ?{i} ESCAPE '\\')",
            i = binds.len()
        ));
    }
    if let Some(shop) = query.shop.as_deref() {
        binds.push(shop.to_string());
        filters.push(format!("t.shop = ?{}", binds.len()));
    }
    let where_clause = if filters.is_empty() {
        "1 = 1".to_string()
    } else {
        filters.join(" AND ")
    };

    let grouped = format!(
        "SELECT t.shop, t.object_id, g.name AS game_name, g.cover_url AS game_cover_url,
                g.fetched_at,
                SUM(t.bytes) AS bytes, SUM(t.cloud_saves) AS cloud_saves,
                SUM(t.backups) AS backups, SUM(t.seconds) AS seconds,
                COUNT(DISTINCT t.user_id) AS players, MAX(t.at) AS last_at
         FROM ({GAME_UNION}) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         WHERE {where_clause}
         GROUP BY t.shop, t.object_id"
    );

    /* Total and how many of it the panel can only show as a raw shop id:
    the screen says so in its own header rather than leaving the operator to
    count warning pills a page at a time. */
    let count_sql = format!(
        "SELECT COUNT(*), SUM(CASE WHEN {unresolved} THEN 1 ELSE 0 END) FROM ({grouped})",
        unresolved = metadata::unresolved_name("game_name"),
    );
    let mut count = sqlx::query_as::<_, (i64, Option<i64>)>(&count_sql);
    for value in &binds {
        count = count.bind(value);
    }
    let (total, unnamed) = count.fetch_one(&state.pool).await?;

    let order = super::order_by(
        &[
            ("storage", "bytes"),
            ("players", "players"),
            ("playtime", "seconds"),
            ("name", "COALESCE(game_name, object_id) COLLATE NOCASE"),
            ("updated", "last_at"),
        ],
        query.sort.as_deref(),
        query.dir.as_deref(),
        "bytes",
    );

    /* Numbered rather than bare, for the reason spelled out in saves.rs. */
    let (limit_slot, offset_slot) = (binds.len() + 1, binds.len() + 2);
    let page_sql = format!("{grouped} ORDER BY {order} LIMIT ?{limit_slot} OFFSET ?{offset_slot}");
    let mut rows = sqlx::query(&page_sql);
    for value in &binds {
        rows = rows.bind(value);
    }
    let rows = rows
        .bind(paging.per_page())
        .bind(paging.offset())
        .fetch_all(&state.pool)
        .await?;

    let games: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "game": super::game_ref(row),
                "bytes": row.get::<i64, _>("bytes"),
                "cloudSaves": row.get::<i64, _>("cloud_saves"),
                "backups": row.get::<i64, _>("backups"),
                "playtimeSeconds": row.get::<i64, _>("seconds"),
                "players": row.get::<i64, _>("players"),
                "lastAt": row.get::<Option<String>, _>("last_at"),
                "metadataFetchedAt": row.get::<Option<String>, _>("fetched_at"),
            })
        })
        .collect();

    let mut envelope = paging.envelope(games, total);
    envelope["unnamed"] = json!(unnamed.unwrap_or(0));

    Ok(Json(envelope))
}

/// GET /admin/api/games/{shop}/{objectId} — one game and everyone who has
/// something stored for it.
///
/// The name/cover lookup is the cached one, so opening a game the panel has
/// never shown resolves it once and every later view is free.
async fn detail(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path((shop, object_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let meta = metadata::resolve(&state, &shop, &object_id).await;

    let players = sqlx::query(&format!(
        "SELECT t.user_id, u.display_name, u.username, u.profile_image_url,
                SUM(t.bytes) AS bytes, SUM(t.cloud_saves) AS cloud_saves,
                SUM(t.backups) AS backups, SUM(t.seconds) AS seconds, MAX(t.at) AS last_at
         FROM ({GAME_UNION}) t
         LEFT JOIN users u ON u.id = t.user_id
         WHERE t.shop = ?1 AND t.object_id = ?2
         GROUP BY t.user_id ORDER BY bytes DESC"
    ))
    .bind(&shop)
    .bind(&object_id)
    .fetch_all(&state.pool)
    .await?;

    let playtime = sqlx::query(
        "SELECT COALESCE(SUM(seconds), 0) AS seconds, COUNT(DISTINCT day) AS days,
                MIN(day) AS first_day, MAX(day) AS last_day
         FROM playtime_daily WHERE shop = ? AND object_id = ?",
    )
    .bind(&shop)
    .bind(&object_id)
    .fetch_one(&state.pool)
    .await?;

    let artwork = sqlx::query(
        "SELECT kind, source, url, size_in_bytes, updated_at, user_id
         FROM game_artwork WHERE shop = ? AND object_id = ? ORDER BY updated_at DESC",
    )
    .bind(&shop)
    .bind(&object_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "game": {
            "shop": shop,
            "objectId": object_id,
            "name": meta.name,
            "coverUrl": meta.cover_url,
        },
        "players": players.iter().map(|row| json!({
            "user": super::user_ref(row),
            "bytes": row.get::<i64, _>("bytes"),
            "cloudSaves": row.get::<i64, _>("cloud_saves"),
            "backups": row.get::<i64, _>("backups"),
            "playtimeSeconds": row.get::<i64, _>("seconds"),
            "lastAt": row.get::<Option<String>, _>("last_at"),
        })).collect::<Vec<_>>(),
        "playtime": {
            "seconds": playtime.get::<i64, _>("seconds"),
            "days": playtime.get::<i64, _>("days"),
            "firstDay": playtime.get::<Option<String>, _>("first_day"),
            "lastDay": playtime.get::<Option<String>, _>("last_day"),
        },
        "artwork": artwork.iter().map(|row| json!({
            "kind": row.get::<String, _>("kind"),
            "source": row.get::<String, _>("source"),
            "url": row.get::<String, _>("url"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "userId": row.get::<String, _>("user_id"),
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
    })))
}

/// POST /admin/api/games/{shop}/{objectId}/refresh — ask the store again,
/// for the game whose name arrived wrong or never arrived at all.
async fn refresh(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path((shop, object_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let meta = metadata::refresh(&state, &shop, &object_id).await;

    Ok(Json(json!({
        "shop": shop,
        "objectId": object_id,
        "name": meta.name,
        "coverUrl": meta.cover_url,
        "resolved": meta.name.is_some(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    /// A game with playtime, and whatever the metadata cache holds for it:
    /// `None` for a game never looked up, `Some` for one that was.
    async fn a_game(server: &TestServer, object_id: &str, name: Option<&str>) {
        let now = chrono::Utc::now().to_rfc3339();
        server
            .execute(&format!(
                "INSERT INTO playtime_daily (user_id, day, shop, object_id, seconds, updated_at)
                 VALUES ('alice', '2026-01-01', 'steam', '{object_id}', 60, '{now}')"
            ))
            .await;

        if let Some(name) = name {
            server
                .execute(&format!(
                    "INSERT INTO game_metadata (shop, object_id, name, fetched_at)
                     VALUES ('steam', '{object_id}', '{name}', '{now}')"
                ))
                .await;
        }
    }

    async fn listing(server: &TestServer) -> Value {
        let Json(value) = list(
            State(server.state.clone()),
            AdminSession { expires_at: 0 },
            Query(ListQuery {
                q: None,
                shop: None,
                sort: None,
                dir: None,
                page: None,
                per_page: None,
            }),
        )
        .await
        .expect("the listing");

        value
    }

    #[tokio::test]
    async fn the_listing_counts_the_games_it_can_only_show_as_an_id() {
        let server = TestServer::start().await;
        a_game(&server, "1", Some("A Game")).await;
        a_game(&server, "2", None).await;
        /* Cached, looked up, and still nameless — the store answered with
        nothing usable. The screen shows this one as its id too. */
        a_game(&server, "3", Some("  ")).await;

        let data = listing(&server).await;

        assert_eq!(data["total"], 3);
        assert_eq!(data["unnamed"], 2);
    }
}
