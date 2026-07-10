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

/// Cached lookup of a game's display metadata by shop/object id.
pub async fn resolve(state: &AppState, shop: &str, object_id: &str) -> GameMetadata {
    if let Ok(Some(row)) = sqlx::query(
        "SELECT name, cover_url, fetched_at FROM game_metadata WHERE shop = ? AND object_id = ?",
    )
    .bind(shop)
    .bind(object_id)
    .fetch_optional(&state.pool)
    .await
    {
        let cached = GameMetadata {
            name: row.get("name"),
            cover_url: row.get("cover_url"),
        };

        let recently_failed = DateTime::parse_from_rfc3339(&row.get::<String, _>("fetched_at"))
            .map(|fetched| {
                Utc::now() - fetched.with_timezone(&Utc)
                    < Duration::hours(RETRY_FAILED_AFTER_HOURS)
            })
            .unwrap_or(true);

        if cached.name.is_some() || recently_failed {
            return cached;
        }
    }

    let metadata = fetch(state, shop, object_id).await;

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

async fn fetch(state: &AppState, shop: &str, object_id: &str) -> GameMetadata {
    match shop {
        "steam" if !object_id.is_empty() && object_id.chars().all(|c| c.is_ascii_digit()) => {
            fetch_steam(state, object_id).await
        }
        /* Other shops have no public metadata endpoint; the panel keeps
           showing the raw shop/object id for them. */
        _ => GameMetadata::default(),
    }
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
            tracing::warn!("steam store returned {} for app {app_id}", response.status());
            None
        }
        Err(err) => {
            tracing::warn!("steam store lookup failed for app {app_id}: {err}");
            None
        }
    };

    GameMetadata { name, cover_url }
}
