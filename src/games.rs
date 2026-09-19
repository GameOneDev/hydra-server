use crate::state::AppState;
use chrono::{DateTime, Duration, Utc};
use sqlx::Row;

/// Game name/cover shown in the admin panel instead of the raw shop id.
#[derive(Clone, Default)]
pub struct GameMetadata {
    pub name: Option<String>,
    pub cover_url: Option<String>,
}

/// Failed name lookups are retried after this long — covers ids the store
/// doesn't know yet (unreleased or delisted games) without hammering it.
const RETRY_FAILED_AFTER_HOURS: i64 = 24;

/// Every `(shop, object_id)` this server holds data for, from every table a
/// game can arrive through — a save of any kind, a backup, playtime,
/// artwork, achievements, a souvenir.
///
/// One definition, because the panel joins `game_metadata` from a dozen
/// screens and the refresh job used to carry a shorter list of its own: a
/// game known only from achievements was never looked up, so the job kept
/// reporting every game named while those screens showed raw ids.
///
/// Event rows are deliberately not here. They are a log — pruned on a
/// schedule, and often about data the server no longer has — so resolving
/// them would mean store lookups for games nothing is stored for.
pub const KNOWN_GAME_IDS: &str = "
    SELECT shop, object_id FROM (
        SELECT shop, object_id FROM cloud_save_snapshots
        UNION SELECT shop, object_id FROM artifacts
        UNION SELECT shop, object_id FROM emulation_saves
        UNION SELECT shop, object_id FROM playtime_daily
        UNION SELECT shop, object_id FROM game_artwork
        UNION SELECT shop, object_id FROM game_achievements
        UNION SELECT shop, object_id FROM souvenirs
    ) WHERE shop IS NOT NULL AND object_id IS NOT NULL AND object_id <> ''
";

/// SQL for "this game still has no name", over whichever column a query
/// selected the cached name into.
///
/// A blank name counts as no name: `admin::game_ref` hands it to the panel,
/// which falls back to the raw object id exactly as it does for a missing
/// row. Testing `name IS NULL` alone left those games looking unresolved on
/// screen and resolved to every query that counted them.
pub fn unresolved_name(column: &str) -> String {
    format!("({column} IS NULL OR TRIM({column}) = '')")
}

/// Whether this server has any way to put a name to an id.
///
/// Only Steam publishes a per-id endpoint, and only for numeric app ids.
/// Everything else keeps showing the raw shop/object id however often it is
/// asked for, so the refresh job counts those separately instead of
/// spending its batch re-asking questions with no answer.
pub fn is_lookupable(shop: &str, object_id: &str) -> bool {
    shop == "steam" && !object_id.is_empty() && object_id.chars().all(|c| c.is_ascii_digit())
}

/// Cached lookup of a game's display metadata by shop/object id.
pub async fn resolve(state: &AppState, shop: &str, object_id: &str) -> GameMetadata {
    let cached = cached(state, shop, object_id).await;

    if let Some((metadata, fetched_at)) = &cached {
        let recently_failed = DateTime::parse_from_rfc3339(fetched_at)
            .map(|fetched| {
                Utc::now() - fetched.with_timezone(&Utc) < Duration::hours(RETRY_FAILED_AFTER_HOURS)
            })
            .unwrap_or(true);

        if metadata.name.is_some() || recently_failed {
            return metadata.clone();
        }
    }

    lookup(state, shop, object_id, cached.map(|(metadata, _)| metadata)).await
}

/// Ask the store again, whatever is cached and however recently the last
/// attempt failed.
///
/// The operator's "Refresh metadata" button on a game, and one entry in the
/// refresh job's batch.
pub async fn refresh(state: &AppState, shop: &str, object_id: &str) -> GameMetadata {
    let previous = cached(state, shop, object_id)
        .await
        .map(|(metadata, _)| metadata);

    lookup(state, shop, object_id, previous).await
}

/// What the cache holds for a game, and when it was written.
async fn cached(state: &AppState, shop: &str, object_id: &str) -> Option<(GameMetadata, String)> {
    let row = sqlx::query(
        "SELECT name, cover_url, fetched_at FROM game_metadata WHERE shop = ? AND object_id = ?",
    )
    .bind(shop)
    .bind(object_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;

    Some((
        GameMetadata {
            name: nonblank(row.get("name")),
            cover_url: nonblank(row.get("cover_url")),
        },
        row.get("fetched_at"),
    ))
}

/// Fetch and cache, keeping what the previous attempt found where this one
/// came back empty.
async fn lookup(
    state: &AppState,
    shop: &str,
    object_id: &str,
    previous: Option<GameMetadata>,
) -> GameMetadata {
    let mut metadata = fetch(state, shop, object_id).await;

    /* A store lookup that fails doesn't unmake the cover the last one found:
    the Steam CDN keeps serving art for ids the store API has forgotten. */
    if metadata.cover_url.is_none() {
        metadata.cover_url = previous.and_then(|previous| previous.cover_url);
    }

    let cached = sqlx::query(
        "INSERT INTO game_metadata (shop, object_id, name, cover_url, fetched_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(shop, object_id) DO UPDATE SET
           name = excluded.name,
           cover_url = excluded.cover_url,
           fetched_at = excluded.fetched_at",
    )
    .bind(shop)
    .bind(object_id)
    .bind(&metadata.name)
    .bind(&metadata.cover_url)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await;

    if let Err(err) = cached {
        tracing::warn!("failed to cache game metadata for {shop}/{object_id}: {err}");
    }

    metadata
}

/// A blank string is not a name (or a cover url) — read and write it as the
/// missing value it is, so one run of the refresh job can replace it.
fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

async fn fetch(state: &AppState, shop: &str, object_id: &str) -> GameMetadata {
    /* Other shops have no public metadata endpoint; the panel keeps showing
    the raw shop/object id for them. */
    if !is_lookupable(shop, object_id) {
        return GameMetadata::default();
    }

    fetch_steam(state, object_id).await
}

async fn fetch_steam(state: &AppState, app_id: &str) -> GameMetadata {
    /* The cover comes straight off the Steam CDN by app id, so it works
    even when the store lookup below fails (e.g. delisted games). */
    let cover_url = Some(format!(
        "https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/{app_id}/capsule_231x87.jpg"
    ));

    let url = format!(
        "https://store.steampowered.com/api/appdetails?appids={app_id}&filters=basic&l=english"
    );

    let name = match state.http.get(&url).send().await {
        Ok(response) if response.status().is_success() => response
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|body| {
                let entry = body.get(app_id)?;
                if !entry.get("success")?.as_bool()? {
                    return None;
                }
                Some(entry.get("data")?.get("name")?.as_str()?.to_string())
            }),
        Ok(response) => {
            tracing::warn!(
                "steam store returned {} for app {app_id}",
                response.status()
            );
            None
        }
        Err(err) => {
            tracing::warn!("steam store lookup failed for app {app_id}: {err}");
            None
        }
    };

    GameMetadata {
        name: nonblank(name),
        cover_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    const COVER: &str = "https://example.invalid/capsule.jpg";

    /// A cache row as a previous lookup would have left it, attempted now so
    /// the retry backoff keeps the test off the network.
    async fn cached_row(server: &TestServer, name: &str, cover_url: &str) {
        server
            .execute(&format!(
                "INSERT INTO game_metadata (shop, object_id, name, cover_url, fetched_at)
                 VALUES ('epic', 'game', '{name}', '{cover_url}', '{}')",
                Utc::now().to_rfc3339()
            ))
            .await;
    }

    #[test]
    fn only_numeric_steam_ids_can_be_looked_up() {
        assert!(is_lookupable("steam", "1238810"));
        assert!(!is_lookupable("steam", ""));
        assert!(!is_lookupable("steam", "not-an-app-id"));
        /* No other shop publishes an endpoint to ask. */
        assert!(!is_lookupable("epic", "1238810"));
    }

    #[test]
    fn the_unresolved_test_catches_blank_names() {
        let sql = unresolved_name("g.name");
        assert!(sql.contains("g.name IS NULL"));
        assert!(sql.contains("TRIM(g.name) = ''"));
    }

    /// The drift this guards against is the one that made the refresh job
    /// claim every game had a name: a table grew a game id, and the query
    /// that was supposed to cover every game was never told about it.
    #[tokio::test]
    async fn every_table_holding_a_game_id_is_covered_or_deliberately_not() {
        /* game_metadata is the cache itself; events are a pruned log of
        things that may no longer exist, and a hidden game is a preference
        the panel never shows a name for. */
        const EXCLUDED: &[&str] = &["game_metadata", "events", "hidden_games"];

        let server = TestServer::start().await;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT m.name FROM sqlite_master m
             WHERE m.type = 'table'
               AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) WHERE name = 'shop')
               AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) WHERE name = 'object_id')",
        )
        .fetch_all(&server.state.pool)
        .await
        .expect("the schema");

        assert!(
            tables.len() > EXCLUDED.len(),
            "the schema query found nothing"
        );

        for table in tables {
            assert!(
                KNOWN_GAME_IDS.contains(&format!("FROM {table}\n"))
                    || EXCLUDED.contains(&table.as_str()),
                "{table} holds a game id no name lookup knows about"
            );
        }
    }

    #[tokio::test]
    async fn a_blank_cached_name_reads_back_as_no_name() {
        let server = TestServer::start().await;
        cached_row(&server, "   ", COVER).await;

        let metadata = resolve(&server.state, "epic", "game").await;

        /* Blank is what the panel already treats as nameless; the cache had
        been handing it back as a name nothing would ever replace. */
        assert!(metadata.name.is_none());
        assert_eq!(metadata.cover_url.as_deref(), Some(COVER));
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_cover_it_had() {
        let server = TestServer::start().await;
        cached_row(&server, "", COVER).await;

        /* 'epic' has no lookup, so this refresh comes back with nothing —
        which is no reason to lose the art the panel is already showing. */
        let metadata = refresh(&server.state, "epic", "game").await;

        assert_eq!(metadata.cover_url.as_deref(), Some(COVER));
        assert_eq!(
            server
                .scalar::<String>("SELECT cover_url FROM game_metadata WHERE shop = 'epic'")
                .await,
            COVER
        );
    }
}
