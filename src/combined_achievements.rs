//! The combined Steam/Epic achievement view (launcher 4.2.0), answered from
//! what this server knows.
//!
//! Official Hydra builds this from its own catalogue: which Steam app and
//! which Epic item are the same game, and which of their achievements are the
//! same achievement. This server has neither, so it rebuilds both:
//!
//! - **Which games** comes from the launcher, which reads the link
//!   (`canonicalId`) off the official profile and sends its library's links
//!   to `PUT /profile/game-links` after every sync.
//! - **Which achievements** is matched by display name, in order: the n-th
//!   achievement called "Victory" on Steam pairs with the n-th one called
//!   "Victory" on Epic. Stores translate names alike, so this holds for the
//!   ordinary case; an achievement whose names differ shows as two rows
//!   rather than being paired with the wrong one.
//!
//! Definitions — names, icons, points — still come from the official
//! catalogue, fetched with the caller's own access token, as the launcher
//! itself would. Unlock times and souvenir pictures are this server's.
//!
//! The launcher asks official Hydra first and comes here only when that gave
//! it nothing, so where the official view is available it wins.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Shops the official combined view covers.
const LINKED_SHOPS: [&str; 2] = ["steam", "epic"];

/// How long a game's definitions are reused before asking again. They change
/// when a game ships achievements, not between page views.
const DEFINITIONS_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const DEFINITIONS_CACHE_LIMIT: usize = 2_000;

type DefinitionsKey = (String, String, String);
type DefinitionsCache = RwLock<HashMap<DefinitionsKey, (Instant, Vec<Value>)>>;

fn definitions_cache() -> &'static DefinitionsCache {
    static CACHE: OnceLock<DefinitionsCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

// ---------------------------------------------------------------------------
// PUT /profile/game-links
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameLink {
    pub shop: String,
    pub object_id: String,
    pub canonical_id: String,
}

/// Replaces the caller's game links with the set the launcher just read off
/// the official profile. The whole set, so a link the catalogue dropped goes
/// too.
pub async fn replace_links(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(links): Json<Vec<GameLink>>,
) -> ApiResult<StatusCode> {
    let sane = |value: &str| {
        !value.is_empty()
            && value.len() <= 255
            && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
    };

    /* Object ids go into the catalogue URL as they are, so only the
    characters Steam app ids and Epic `namespace:item` ids use. */
    let plain_id = |value: &str| {
        sane(value)
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
    };

    let now = Utc::now().to_rfc3339();
    let mut tx = state.pool.begin().await?;

    sqlx::query("DELETE FROM game_links WHERE user_id = ?")
        .bind(&user.0.id)
        .execute(&mut *tx)
        .await?;

    for link in links.iter().filter(|link| {
        LINKED_SHOPS.contains(&link.shop.as_str())
            && plain_id(&link.object_id)
            && sane(&link.canonical_id)
    }) {
        sqlx::query(
            "INSERT OR REPLACE INTO game_links (user_id, shop, object_id, canonical_id, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&user.0.id)
        .bind(&link.shop)
        .bind(&link.object_id)
        .bind(&link.canonical_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /profile/achievements/combined
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedQuery {
    pub shop: String,
    pub object_id: String,
    #[serde(default)]
    pub language: Option<String>,
}

/// One store's copy of a linked game, with everything the view needs.
pub(crate) struct Copy {
    pub shop: String,
    pub object_id: String,
    pub definitions: Vec<Value>,
    /// Upper-cased achievement name -> unlock time, epoch milliseconds.
    pub unlocks: HashMap<String, i64>,
    /// Upper-cased achievement name -> souvenir picture.
    pub images: HashMap<String, String>,
}

pub async fn combined(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Query(query): Query<CombinedQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = &user.0.id;
    let language = query
        .language
        .filter(|language| !language.is_empty() && language.len() <= 16)
        .unwrap_or_else(|| "en".to_string());

    /* A 404 sends the launcher to the game's own list, which for an unlinked
    game is the same thing this would have said. */
    let canonical_id: String = sqlx::query_scalar(
        "SELECT canonical_id FROM game_links WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(&query.shop)
    .bind(&query.object_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("this game isn't linked to another store"))?;

    let linked = sqlx::query(
        "SELECT shop, object_id FROM game_links
         WHERE user_id = ? AND canonical_id = ? AND shop IN ('steam', 'epic')
         ORDER BY shop = 'steam' DESC, object_id",
    )
    .bind(user_id)
    .bind(&canonical_id)
    .fetch_all(&state.pool)
    .await?;

    if linked.len() < 2 {
        return Err(ApiError::not_found(
            "this game isn't linked to another store",
        ));
    }

    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();

    let mut copies = Vec::with_capacity(linked.len());
    for row in &linked {
        let shop: String = row.get("shop");
        let object_id: String = row.get("object_id");

        copies.push(Copy {
            definitions: definitions(&state, &token, &shop, &object_id, &language).await,
            unlocks: unlocks(&state, user_id, &shop, &object_id).await?,
            images: crate::souvenirs::images_by_achievement(
                &state, user_id, &shop, &object_id, true,
            )
            .await?,
            shop,
            object_id,
        });
    }

    let achievements = combine(&canonical_id, copies);
    if achievements.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "no achievement definitions for these games",
        ));
    }

    Ok(Json(json!({
        "canonicalId": canonical_id,
        "achievements": achievements,
    })))
}

/// A game's achievement definitions from the official catalogue, cached.
/// Empty when the catalogue has none or can't be reached.
async fn definitions(
    state: &AppState,
    token: &str,
    shop: &str,
    object_id: &str,
    language: &str,
) -> Vec<Value> {
    let key = (
        shop.to_string(),
        object_id.to_string(),
        language.to_string(),
    );

    if let Some((fetched, cached)) = definitions_cache().read().await.get(&key) {
        if fetched.elapsed() < DEFINITIONS_TTL {
            return cached.clone();
        }
    }

    let base = state.config.official_api_url.trim_end_matches('/');
    if base.is_empty() || token.is_empty() {
        return Vec::new();
    }

    let url = format!("{base}/games/{shop}/{object_id}/achievements");
    let fetched = match state
        .http
        .get(&url)
        .query(&[("language", language)])
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response.json::<Vec<Value>>().await.ok(),
        Ok(response) => {
            tracing::warn!(
                "official API returned {} for {shop}/{object_id} achievements",
                response.status()
            );
            None
        }
        Err(err) => {
            tracing::warn!("achievement definitions lookup failed for {shop}/{object_id}: {err}");
            None
        }
    };

    /* A failed lookup isn't cached: the next view asks again. */
    let Some(definitions) = fetched else {
        return Vec::new();
    };

    let mut cache = definitions_cache().write().await;
    if cache.len() >= DEFINITIONS_CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), definitions.clone()));

    definitions
}

/// For tests: what a catalogue lookup would have returned.
#[cfg(test)]
pub(crate) async fn seed_definitions(
    shop: &str,
    object_id: &str,
    language: &str,
    values: Vec<Value>,
) {
    definitions_cache().write().await.insert(
        (
            shop.to_string(),
            object_id.to_string(),
            language.to_string(),
        ),
        (Instant::now(), values),
    );
}

/// The caller's unlocks for one game, from every sync row this server holds.
async fn unlocks(
    state: &AppState,
    user_id: &str,
    shop: &str,
    object_id: &str,
) -> ApiResult<HashMap<String, i64>> {
    let rows = sqlx::query(
        "SELECT achievements FROM game_achievements
         WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(shop)
    .bind(object_id)
    .fetch_all(&state.pool)
    .await?;

    let mut unlocks: HashMap<String, i64> = HashMap::new();
    for row in rows {
        let achievements: Vec<Value> =
            serde_json::from_str(&row.get::<String, _>("achievements")).unwrap_or_default();

        for achievement in &achievements {
            let (Some(name), Some(time)) = (
                achievement.get("name").and_then(Value::as_str),
                crate::achievements::unlock_time(achievement),
            ) else {
                continue;
            };

            let entry = unlocks.entry(name.to_uppercase()).or_insert(time);
            *entry = (*entry).min(time);
        }
    }

    Ok(unlocks)
}

/// What two achievements must share to be the same one: their display name,
/// ignoring case, spacing and punctuation. Falls back to the internal name
/// for an achievement with no display name.
fn pairing_key(definition: &Value) -> Option<String> {
    let display: String = definition
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();

    if !display.is_empty() {
        return Some(display);
    }

    let name = definition.get("name").and_then(Value::as_str)?;
    Some(format!("name:{}", name.to_uppercase()))
}

/// Groups the copies' achievements into the official response's shape.
///
/// A copy with no definitions of its own borrows those of the first copy
/// that has them — the launcher does the same for an Epic copy, whose unlocks
/// then carry the Steam names. Groups keep the order the first copy lists
/// them in.
pub(crate) fn combine(canonical_id: &str, copies: Vec<Copy>) -> Vec<Value> {
    let fallback = copies
        .iter()
        .find(|copy| !copy.definitions.is_empty())
        .map(|copy| copy.definitions.clone())
        .unwrap_or_default();

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();

    for copy in &copies {
        let definitions = if copy.definitions.is_empty() {
            &fallback
        } else {
            &copy.definitions
        };

        /* The n-th achievement with a given name pairs with the n-th one of
        that name in the other store, so repeated names stay distinct. */
        let mut seen: HashMap<String, usize> = HashMap::new();

        for definition in definitions {
            let (Some(name), Some(base_key)) = (
                definition.get("name").and_then(Value::as_str),
                pairing_key(definition),
            ) else {
                continue;
            };

            let occurrence = seen.entry(base_key.clone()).or_insert(0);
            let key = format!("{base_key}#{occurrence}");
            *occurrence += 1;

            let upper = name.to_uppercase();
            let text = |field: &str| definition.get(field).and_then(Value::as_str);
            let icon = text("icon").unwrap_or_default();

            let mut variant = json!({
                "shop": copy.shop,
                "objectId": copy.object_id,
                "name": name,
                "displayName": text("displayName").filter(|value| !value.is_empty()).unwrap_or(name),
                "hidden": definition.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                "icon": icon,
                "icongray": text("icongray").unwrap_or(icon),
                /* Epic unlocks arrive named by Epic's own id, which the
                catalogue keeps as `externalId` beside the name. */
                "unlockTime": copy.unlocks.get(&upper).or_else(|| {
                    text("externalId").and_then(|id| copy.unlocks.get(&id.to_uppercase()))
                }),
                "imageUrl": copy.images.get(&upper),
            });
            if let Some(description) = text("description") {
                variant["description"] = json!(description);
            }
            if let Some(points) = definition.get("points").filter(|value| value.is_number()) {
                variant["points"] = points.clone();
            }

            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(variant);
        }
    }

    order
        .into_iter()
        .filter_map(|key| {
            let variants = groups.remove(&key)?;
            Some(json!({
                "canonicalAchievementId": format!("{canonical_id}:{key}"),
                "variants": variants,
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    fn definition(name: &str, display: &str) -> Value {
        json!({
            "name": name,
            "displayName": display,
            "description": format!("{display}!"),
            "icon": format!("https://cdn.example/{name}.jpg"),
            "icongray": format!("https://cdn.example/{name}_gray.jpg"),
            "hidden": false,
            "points": 10,
        })
    }

    fn copy(shop: &str, object_id: &str, definitions: Vec<Value>) -> Copy {
        Copy {
            shop: shop.to_string(),
            object_id: object_id.to_string(),
            definitions,
            unlocks: HashMap::new(),
            images: HashMap::new(),
        }
    }

    #[test]
    fn the_same_achievement_in_both_stores_is_one_group() {
        let mut steam = copy(
            "steam",
            "570",
            vec![
                definition("ACH_WIN", "Victory!"),
                definition("ACH_LOSE", "Defeat"),
            ],
        );
        steam
            .images
            .insert("ACH_WIN".into(), "https://srv/win.jpg".into());
        let mut epic = copy(
            "epic",
            "ns:item",
            vec![
                definition("victory_epic", "VICTORY"),
                definition("only_epic", "Epic only"),
            ],
        );
        epic.unlocks.insert("VICTORY_EPIC".into(), 1_700);

        let groups = combine("canon", vec![steam, epic]);

        assert_eq!(groups.len(), 3);
        let victory = &groups[0]["variants"];
        assert_eq!(victory.as_array().unwrap().len(), 2);
        assert_eq!(victory[0]["shop"], "steam");
        assert_eq!(victory[0]["imageUrl"], "https://srv/win.jpg");
        assert_eq!(victory[0]["unlockTime"], Value::Null);
        assert_eq!(victory[1]["name"], "victory_epic");
        assert_eq!(victory[1]["unlockTime"], 1_700);
        assert_eq!(groups[0]["canonicalAchievementId"], "canon:victory#0");
        /* Unmatched on either side, each stays a group of its own. */
        assert_eq!(groups[1]["variants"].as_array().unwrap().len(), 1);
        assert_eq!(groups[2]["variants"][0]["shop"], "epic");
    }

    #[test]
    fn epic_unlocks_are_found_by_their_external_id() {
        let mut epic = copy(
            "epic",
            "ns:1",
            vec![json!({ "name": "win", "displayName": "Win", "externalId": "ach_win_9" })],
        );
        epic.unlocks.insert("ACH_WIN_9".into(), 77);

        let groups = combine("c", vec![epic]);

        assert_eq!(groups[0]["variants"][0]["unlockTime"], 77);
    }

    #[test]
    fn repeated_names_pair_in_order() {
        let steam = copy(
            "steam",
            "1",
            vec![definition("A1", "Chapter"), definition("A2", "Chapter")],
        );
        let epic = copy(
            "epic",
            "ns:1",
            vec![definition("e1", "Chapter"), definition("e2", "Chapter")],
        );

        let groups = combine("c", vec![steam, epic]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["variants"][1]["name"], "e1");
        assert_eq!(groups[1]["variants"][1]["name"], "e2");
    }

    #[test]
    fn a_copy_without_definitions_borrows_them() {
        let steam = copy("steam", "1", vec![definition("ACH_WIN", "Victory")]);
        let mut epic = copy("epic", "ns:1", vec![]);
        epic.unlocks.insert("ACH_WIN".into(), 42);

        let groups = combine("c", vec![steam, epic]);

        assert_eq!(groups[0]["variants"][1]["shop"], "epic");
        assert_eq!(groups[0]["variants"][1]["unlockTime"], 42);
    }

    #[tokio::test]
    async fn the_view_needs_two_linked_copies() {
        let server = TestServer::start().await;
        server
            .execute(
                "INSERT INTO game_links VALUES
                   ('alice', 'steam', '9001', 'canon-9001', '2026-01-01T00:00:00Z')",
            )
            .await;

        let ask = |shop: &str, object_id: &str| {
            combined(
                State(server.state.clone()),
                server.user("alice"),
                HeaderMap::new(),
                Query(CombinedQuery {
                    shop: shop.to_string(),
                    object_id: object_id.to_string(),
                    language: Some("en".to_string()),
                }),
            )
        };

        let Err(unlinked) = ask("steam", "9002").await else {
            panic!("a game with no link");
        };
        assert_eq!(unlinked.status, StatusCode::NOT_FOUND);

        let Err(alone) = ask("steam", "9001").await else {
            panic!("a link to nothing else");
        };
        assert_eq!(alone.status, StatusCode::NOT_FOUND);

        server
            .execute(
                "INSERT INTO game_links VALUES
                   ('alice', 'epic', 'ns:9001', 'canon-9001', '2026-01-01T00:00:00Z')",
            )
            .await;
        server
            .execute(
                "INSERT INTO game_achievements (user_id, remote_game_id, shop, object_id, achievements, updated_at)
                 VALUES ('alice', 'r1', 'epic', 'ns:9001',
                         '[{\"name\":\"WIN_EPIC\",\"unlockTime\":5}]', '2026-01-01T00:00:00Z')",
            )
            .await;
        seed_definitions("steam", "9001", "en", vec![definition("WIN", "Win")]).await;
        seed_definitions("epic", "ns:9001", "en", vec![definition("WIN_EPIC", "Win")]).await;

        let Json(view) = ask("epic", "ns:9001").await.expect("the combined view");

        assert_eq!(view["canonicalId"], "canon-9001");
        let variants = &view["achievements"][0]["variants"];
        assert_eq!(variants[0]["shop"], "steam");
        assert_eq!(variants[0]["unlockTime"], Value::Null);
        assert_eq!(variants[1]["unlockTime"], 5);
    }

    #[tokio::test]
    async fn links_are_replaced_wholesale_and_filtered() {
        let server = TestServer::start().await;

        let send = |links: Value| {
            replace_links(
                State(server.state.clone()),
                server.user("alice"),
                Json(serde_json::from_value(links).expect("links")),
            )
        };

        send(json!([
            { "shop": "steam", "objectId": "1", "canonicalId": "c1" },
            { "shop": "epic", "objectId": "ns:1", "canonicalId": "c1" },
            { "shop": "launchbox", "objectId": "x", "canonicalId": "c1" },
            { "shop": "steam", "objectId": "1/../../profile/me", "canonicalId": "c1" },
        ]))
        .await
        .expect("the first set");
        assert_eq!(
            server
                .scalar::<i64>("SELECT COUNT(*) FROM game_links")
                .await,
            2
        );

        send(json!([{ "shop": "steam", "objectId": "2", "canonicalId": "c2" }]))
            .await
            .expect("the second set");
        assert_eq!(
            server
                .scalar::<String>("SELECT object_id FROM game_links")
                .await,
            "2"
        );
    }
}
