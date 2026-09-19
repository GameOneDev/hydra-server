use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::games;
use crate::query::shops_from_query;
use crate::state::AppState;
use axum::extract::{Path, Query, RawQuery, State};
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
    /// Whether the Steam integration is what brought this game in, which is
    /// how [`user_stats`] answers a profile filtered to the Steam library.
    /// Absent leaves whatever is already stored.
    #[serde(default)]
    pub has_active_steam_import: Option<bool>,
    #[serde(default)]
    pub achievements: Vec<Value>,
    /// Screenshots captured alongside these unlocks, already uploaded by the
    /// time they arrive here. See [`crate::souvenirs`].
    #[serde(default)]
    pub souvenirs: Vec<crate::souvenirs::SyncSouvenir>,
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
///
/// Names are compared case-insensitively, as the launcher does: the same
/// achievement can be recorded with different casing depending on where it
/// was read from, and matching exactly would store it twice.
fn merge_achievements(existing: Vec<Value>, incoming: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(existing.len() + incoming.len());

    for achievement in existing.into_iter().chain(incoming) {
        let Some(name) = achievement_name(&achievement).map(str::to_uppercase) else {
            continue;
        };

        match merged
            .iter_mut()
            .find(|entry| achievement_name(entry).map(str::to_uppercase) == Some(name.clone()))
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
///
/// A payload carrying souvenirs always gets a body: the launcher only stops
/// retrying one once it reads its own client id back out of `souvenirs`.
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
        .and_then(|row| serde_json::from_str(&row.get::<String, _>("achievements")).ok())
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
        "INSERT INTO game_achievements (user_id, remote_game_id, shop, object_id, achievements, has_active_steam_import, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, remote_game_id) DO UPDATE SET
           shop = COALESCE(excluded.shop, game_achievements.shop),
           object_id = COALESCE(excluded.object_id, game_achievements.object_id),
           achievements = excluded.achievements,
           has_active_steam_import = COALESCE(
             excluded.has_active_steam_import,
             game_achievements.has_active_steam_import
           ),
           updated_at = excluded.updated_at",
    )
    .bind(&user.0.id)
    .bind(&payload.id)
    .bind(&shop)
    .bind(&object_id)
    .bind(&merged_json)
    .bind(payload.has_active_steam_import)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    let souvenirs = crate::souvenirs::claim_from_sync(
        &state,
        &user.0.id,
        crate::souvenirs::SyncGame {
            remote_id: &payload.id,
            shop: shop.as_deref(),
            object_id: object_id.as_deref(),
        },
        &payload.souvenirs,
        &merged,
    )
    .await?;

    match (object_id, shop) {
        (Some(object_id), Some(shop)) => Ok(Json(json!({
            "objectId": object_id,
            "shop": shop,
            "achievements": merged,
            "souvenirs": souvenirs,
        }))
        .into_response()),
        /* Without shop/objectId the launcher can't repaint its local state
        from this response, but an acknowledged souvenir still has to be
        reported — losing it would leave the launcher retrying a souvenir
        this server already stored. */
        _ if !souvenirs.is_empty() => Ok(Json(json!({
            "objectId": Value::Null,
            "shop": Value::Null,
            "achievements": merged,
            "souvenirs": souvenirs,
        }))
        .into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserStatsQuery {
    /// Set by the launcher's "Steam library" tab. `shop` is repeated rather
    /// than listed, so it is read from the raw query string instead.
    pub steam_library: Option<bool>,
}

/// Whether a stored game belongs to the slice of the library the profile is
/// showing.
///
/// `shops` is what the launcher's platform tabs send
/// (`?shop=steam&shop=launchbox` for all of it, `?shop=launchbox` for
/// Classics), and `steam_library` narrows that to the games the Steam
/// integration imported.
fn counts_toward_stats(
    shop: Option<&str>,
    has_active_steam_import: Option<bool>,
    shops: &[String],
    steam_library: bool,
) -> bool {
    /* A game that has not synced since this server started storing the flag
    reads as "not imported": claiming the Steam tab for it would inflate a
    total the official API computes from the import itself. */
    if steam_library && has_active_steam_import != Some(true) {
        return false;
    }

    if shops.is_empty() {
        return true;
    }

    match shop {
        Some(shop) => shops.contains(&shop.to_lowercase()),
        /* Rows predating the launcher sending shop/objectId can't be placed
        in a tab. Dropping them would understate the library-wide total the
        default tab shows, which is the number nearly everyone reads. */
        None => true,
    }
}

/// GET /profile/stats/{userId} — achievement-count fallback.
///
/// The official API only computes profile achievement totals for
/// subscribers; launchers fill the gap from the achievements synced here.
/// Returns null when the user has no achievement data on this server so
/// clients don't render a misleading zero.
///
/// The launcher's profile tabs pass their filter along (`?shop=…`,
/// `?steamLibrary=true`), so the total follows the tab rather than always
/// answering for the whole library.
pub async fn user_stats(
    State(state): State<AppState>,
    _viewer: CurrentUser,
    Path(user_id): Path<String>,
    Query(query): Query<UserStatsQuery>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let shops = shops_from_query(raw.as_deref());
    let steam_library = query.steam_library.unwrap_or(false);

    let rows = sqlx::query(
        "SELECT shop, object_id, has_active_steam_import, json_array_length(achievements) AS cnt
         FROM game_achievements WHERE user_id = ?",
    )
    .bind(&user_id)
    .fetch_all(&state.pool)
    .await?;

    if rows.is_empty() {
        return Ok(Json(json!({ "unlockedAchievementSum": null })));
    }

    let hidden = crate::hidden_games::hidden_set(&state.pool, &user_id).await?;

    // shop and object_id are nullable. A row synced without them cannot be
    // matched against the hidden list, but its achievements still count.
    let sum: i64 = rows
        .iter()
        .filter(|row| {
            let shop: Option<&str> = row.get("shop");
            let object_id: Option<&str> = row.get("object_id");

            match (shop, object_id) {
                (Some(shop), Some(object_id)) => !hidden.contains(shop, object_id),
                _ => true,
            }
        })
        .filter(|row| {
            counts_toward_stats(
                row.get("shop"),
                row.get("has_active_steam_import"),
                &shops,
                steam_library,
            )
        })
        .map(|row| row.get::<i64, _>("cnt"))
        .sum();

    Ok(Json(json!({ "unlockedAchievementSum": Some(sum) })))
}

/// How many games' worth of recent unlocks a profile view gets back.
const RECENT_GAMES_LIMIT: usize = 6;

/// How many achievements are kept per game.
///
/// The launcher renders only a couple, but it also counts what it receives
/// to show "(N new)" beside the game — so trimming close to what's displayed
/// makes that number read as the cap rather than the real total. This is a
/// safety bound on the response size, not a display limit: it sits well
/// above any real game's achievement count so it never truncates in
/// practice.
const RECENT_ACHIEVEMENTS_PER_GAME: usize = 500;

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
    let unlocked_count = unlocked.len();

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
            /* Total unlocked, not the trimmed list, so the launcher shows a
               true count next to the game. */
            "unlockedCount": unlocked_count,
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

    let hidden = crate::hidden_games::hidden_set(&state.pool, &user_id).await?;

    let mut games: Vec<(i64, Value)> = rows
        .iter()
        .filter(|row| !hidden.contains(row.get("shop"), row.get("object_id")))
        .filter_map(|row| {
            let achievements: Vec<Value> =
                serde_json::from_str(&row.get::<String, _>("achievements")).ok()?;

            recent_game(row.get("shop"), row.get("object_id"), &achievements)
        })
        .collect();

    games.sort_by_key(|(most_recent, _)| std::cmp::Reverse(*most_recent));
    games.truncate(RECENT_GAMES_LIMIT);

    /* The viewer may not have the game in their own library, and the owner's
    library isn't always readable, so the name and cover ride along from
    the metadata cache. Without them the launcher has unlocks it can't
    label and has to drop. Bounded by RECENT_GAMES_LIMIT, and cached after
    the first lookup. */
    let mut resolved = Vec::with_capacity(games.len());
    for (_, mut game) in games {
        let shop = game["shop"].as_str().unwrap_or_default().to_string();
        let object_id = game["objectId"].as_str().unwrap_or_default().to_string();
        let metadata = games::resolve(&state, &shop, &object_id).await;

        game["title"] = json!(metadata.name);
        game["coverUrl"] = json!(metadata.cover_url);
        resolved.push(game);
    }

    Ok(Json(json!({ "games": resolved })))
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
    use crate::state::CachedUser;
    use crate::testing::TestServer;

    const TOKEN: &str = "alices-access-token";

    /// A token the auth extractor accepts. Seeding the cache is what the
    /// first verified request would leave behind, and keeps the test off the
    /// official API.
    async fn authorize(server: &TestServer) {
        server.state.token_cache.write().await.insert(
            TOKEN.to_string(),
            CachedUser {
                user: server.user("alice").0,
                cached_at: Utc::now(),
            },
        );
    }

    /// The assembled router on a loopback port. The stats filter is half
    /// query-string parsing — `shop` repeats, `steamLibrary` does not — so it
    /// is worth driving over a real request rather than calling the handler.
    async fn serve(server: &TestServer) -> String {
        let app = crate::router(server.state.clone()).with_state(server.state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the bound address")
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("the test server");
        });

        base
    }

    fn client() -> reqwest::Client {
        /* The proxy this may run behind has no business intercepting loopback. */
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an http client")
    }

    async fn sync_game(
        client: &reqwest::Client,
        base: &str,
        object_id: &str,
        shop: &str,
        has_active_steam_import: bool,
        unlocked: usize,
    ) {
        let achievements: Vec<Value> = (0..unlocked)
            .map(|index| json!({ "name": format!("{object_id}_ACH_{index}"), "unlockTime": 100 }))
            .collect();

        let response = client
            .put(format!("{base}/profile/games/achievements"))
            .bearer_auth(TOKEN)
            .json(&json!({
                "id": format!("remote-{object_id}"),
                "objectId": object_id,
                "shop": shop,
                "hasActiveSteamImport": has_active_steam_import,
                "achievements": achievements,
            }))
            .send()
            .await
            .expect("a response");

        assert_eq!(response.status(), 200, "sync of {object_id}");
    }

    async fn unlocked_sum(client: &reqwest::Client, base: &str, query: &str) -> Value {
        let response = client
            .get(format!("{base}/profile/stats/alice{query}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("a response");

        assert_eq!(response.status(), 200, "stats for {query}");

        response.json::<Value>().await.expect("a stats body")["unlockedAchievementSum"].clone()
    }

    /// The launcher's profile tabs each send their own filter, and the
    /// achievement total has to follow the tab the way the rest of the stats
    /// do — including "Steam library", which only this server's own record of
    /// the import can answer.
    #[tokio::test]
    async fn the_total_follows_the_tab_the_profile_is_showing() {
        let server = TestServer::start().await;
        authorize(&server).await;
        let base = serve(&server).await;
        let client = client();

        sync_game(&client, &base, "440", "steam", false, 3).await;
        sync_game(&client, &base, "570", "steam", true, 5).await;
        sync_game(&client, &base, "snes-1", "launchbox", false, 2).await;

        assert_eq!(unlocked_sum(&client, &base, "").await, json!(10));
        assert_eq!(
            unlocked_sum(&client, &base, "?shop=steam&shop=launchbox").await,
            json!(10)
        );
        assert_eq!(unlocked_sum(&client, &base, "?shop=steam").await, json!(8));
        assert_eq!(
            unlocked_sum(&client, &base, "?shop=launchbox").await,
            json!(2)
        );
        assert_eq!(
            unlocked_sum(&client, &base, "?shop=steam&steamLibrary=true").await,
            json!(5)
        );
    }

    /// A sync that doesn't mention the import — the souvenir worker's, when
    /// it can't read the game — must not erase what a previous one recorded.
    #[tokio::test]
    async fn a_sync_without_the_flag_keeps_the_stored_one() {
        let server = TestServer::start().await;
        authorize(&server).await;
        let base = serve(&server).await;
        let client = client();

        sync_game(&client, &base, "570", "steam", true, 1).await;

        let response = client
            .put(format!("{base}/profile/games/achievements"))
            .bearer_auth(TOKEN)
            .json(&json!({
                "id": "remote-570",
                "achievements": [{ "name": "570_ACH_1", "unlockTime": 200 }],
            }))
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status(), 200);

        assert_eq!(
            unlocked_sum(&client, &base, "?shop=steam&steamLibrary=true").await,
            json!(2)
        );
    }

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

        let (most_recent, _) = recent_game("steam".into(), "440".into(), &legacy).expect("game");

        assert_eq!(most_recent, 1700);
    }

    #[test]
    fn counts_every_game_when_no_tab_filter_is_sent() {
        assert!(counts_toward_stats(Some("steam"), None, &[], false));
        assert!(counts_toward_stats(None, None, &[], false));
    }

    #[test]
    fn counts_only_the_shops_the_tab_asks_for() {
        let classics = vec!["launchbox".to_string()];

        assert!(counts_toward_stats(
            Some("launchbox"),
            None,
            &classics,
            false
        ));
        assert!(!counts_toward_stats(Some("steam"), None, &classics, false));
    }

    /// The default tab sends both shops, and a row from before the launcher
    /// keyed achievements by game has no shop to match — counting it keeps
    /// that total the same as it was before the tabs existed.
    #[test]
    fn counts_a_row_that_predates_game_keys() {
        let all = vec!["steam".to_string(), "launchbox".to_string()];

        assert!(counts_toward_stats(None, None, &all, false));
    }

    #[test]
    fn steam_library_counts_only_imported_games() {
        let steam = vec!["steam".to_string()];

        assert!(counts_toward_stats(Some("steam"), Some(true), &steam, true));
        assert!(!counts_toward_stats(
            Some("steam"),
            Some(false),
            &steam,
            true
        ));
        /* Not synced since the flag existed: unknown, so not claimed. */
        assert!(!counts_toward_stats(Some("steam"), None, &steam, true));
    }

    /// The PC and All tabs don't exclude imported games — the launcher shows
    /// them there too.
    #[test]
    fn an_imported_game_still_counts_outside_the_steam_tab() {
        let pc = vec!["steam".to_string()];

        assert!(counts_toward_stats(Some("steam"), Some(true), &pc, false));
    }

    /// The same achievement read from different sources can differ in
    /// casing; matching exactly would store it twice.
    #[test]
    fn merge_treats_differently_cased_names_as_one_achievement() {
        let merged = merge_achievements(
            vec![json!({ "name": "ach_win_one_game", "unlockTime": 300 })],
            vec![json!({ "name": "ACH_WIN_ONE_GAME", "unlockTime": 100 })],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(unlocked_at(&merged[0]), 100);
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

    /// The launcher counts what it receives to show "(N new)", so a real
    /// game's unlocks must arrive whole rather than trimmed to the cap.
    #[test]
    fn recent_game_keeps_every_unlock_of_a_realistic_game() {
        let achievements: Vec<Value> = (0..104)
            .map(|i| json!({ "name": format!("ACH_{i}"), "unlockTime": 1000 + i }))
            .collect();

        let (_, game) = recent_game("steam".into(), "648800".into(), &achievements).expect("game");

        assert_eq!(game["achievements"].as_array().unwrap().len(), 104);
        assert_eq!(game["unlockedCount"], 104);
    }

    #[test]
    fn recent_game_skips_games_with_nothing_unlocked() {
        let achievements = vec![json!({ "name": "LOCKED" })];

        assert!(recent_game("steam".into(), "440".into(), &achievements).is_none());
    }
}
