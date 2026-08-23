//! Achievement souvenirs (upstream hydralauncher/hydra#2700).
//!
//! A screenshot taken when an achievement pops, filed on the player's profile.
//! Achievements that unlock together share one picture, so the picture is the
//! record and the names hang off it.
//!
//! Uploading takes three calls, because the row exists before the bytes do:
//!
//! 1. `POST /presigned-urls/achievement-image` reserves the capture's
//!    `clientId` and answers with the storage key plus a presigned PUT.
//! 2. The launcher PUTs the bytes to `/storage/{token}`.
//! 3. `PUT /profile/games/achievements` carries the souvenir next to its
//!    achievements, promoting the reservation. That response **must** echo
//!    `souvenirs: [{ clientId, id }]` or the launcher retries forever.
//!
//! The launcher retries the whole sequence under the same `clientId`, so every
//! step here is idempotent.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::games;
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use uuid::Uuid;

const MAX_SOUVENIR_BYTES: i64 = 20 * 1024 * 1024;

const ALLOWED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// The launcher's own cap. A longer list is truncated rather than refused —
/// refusing it makes the launcher rotate its id and re-upload, on a loop.
const MAX_ACHIEVEMENTS_PER_SOUVENIR: usize = 50;

/// The launcher's `SOUVENIRS_PAGE_SIZE`.
const DEFAULT_PAGE_SIZE: i64 = 24;
const MAX_PAGE_SIZE: i64 = 100;

/// Must match `UPLOAD_TOKEN_TTL_SECONDS` in [`crate::storage`]; reported to
/// the launcher as `expiresAt`.
const UPLOAD_TTL_SECONDS: i64 = 60 * 60;

const ALLOWED_REPORT_REASONS: &[&str] =
    &["hate", "sexual_content", "violence", "spam", "other"];

const REPORT_RATE_LIMIT_PER_HOUR: i64 = 30;

/// Codes the launcher's retry policy matches on. The wrong one doesn't just
/// mislabel a failure, it picks the wrong recovery — a souvenir that needed a
/// retry gets abandoned instead.
const CONFLICT_CODE: &str = "achievements/souvenir-conflict";
const UPLOAD_INCOMPLETE_CODE: &str = "achievements/souvenir-upload-incomplete";

/// The `clientId` rides along so the launcher can tell a failure about *this*
/// capture from one about another.
fn conflict(reason: &str, client_id: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, CONFLICT_CODE).with_extra(json!({
        "reason": reason,
        "clientId": client_id,
    }))
}

fn storage_key(user_id: &str, file_name: &str) -> String {
    format!("images/souvenirs/{user_id}/{file_name}")
}

/// Images live under the owner's own prefix, so a payload can't claim someone
/// else's key.
fn key_belongs_to(user_id: &str, key: &str) -> bool {
    key.starts_with(&format!("images/souvenirs/{user_id}/"))
        && !key.contains("..")
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

fn public_url(state: &AppState, key: &str) -> String {
    format!(
        "{}/{}",
        state.config.public_url,
        key.trim_start_matches('/')
    )
}

fn normalize_visibility(value: &str) -> Option<&'static str> {
    match value.to_uppercase().as_str() {
        "PUBLIC" => Some("PUBLIC"),
        "PRIVATE" => Some("PRIVATE"),
        "FRIENDS" => Some("FRIENDS"),
        _ => None,
    }
}

/// Upper-cased throughout: the launcher reads names out of achievement files,
/// whose casing doesn't reliably match the catalogue's.
fn normalize_names(names: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .iter()
        .map(|name| name.trim().to_uppercase())
        .filter(|name| !name.is_empty() && seen.insert(name.clone()))
        .collect()
}

fn parse_names(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Upload authorization
// ---------------------------------------------------------------------------

/// `POST /presigned-urls/achievement-image`. The pre-grouped upload flow sent
/// no `clientId`, so one is invented for it — the bytes stay accounted for,
/// they just can't be deduplicated across retries.
pub struct AuthorizeRequest<'a> {
    pub image_ext: &'a str,
    pub image_length: i64,
    pub client_id: Option<&'a str>,
    pub remote_game_id: Option<&'a str>,
}

/// POST /presigned-urls/achievement-image — reserves a capture.
///
/// Answers `pending` with a URL to upload to, or `claimed` when the bytes are
/// already here and the launcher should go straight to the sync. The same
/// `clientId` always gets the same key, so a retry doesn't strand an upload.
pub async fn authorize(
    state: &AppState,
    user_id: &str,
    request: AuthorizeRequest<'_>,
) -> ApiResult<Json<Value>> {
    let ext = request
        .image_ext
        .trim()
        .trim_start_matches('.')
        .to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ApiError::bad_request("unsupported image format"));
    }

    /* A declared size is mandatory: `sign_upload_url` needs a real limit to
       bind the token to, and it is also what the quota below is checked
       against. The launcher stats the file before asking. */
    let limit = storage::upload_limit(request.image_length)
        .ok_or_else(|| ApiError::bad_request("imageLength is required"))?;

    if request.image_length > MAX_SOUVENIR_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "screenshot is too large",
        ));
    }

    let length = request.image_length;
    let client_id = request
        .client_id
        .map(str::to_string)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let existing = sqlx::query(
        "SELECT id, image_key, is_uploaded FROM souvenirs
         WHERE user_id = ? AND client_id = ?",
    )
    .bind(user_id)
    .bind(&client_id)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(row) = existing {
        let image_key: String = row.get("image_key");

        if row.get::<i64, _>("is_uploaded") == 1 {
            return Ok(Json(json!({
                "imageKey": image_key,
                "presignedUrl": null,
                "status": "claimed",
                "expiresAt": null,
            })));
        }

        return Ok(Json(authorization(state, &image_key, limit)));
    }

    /* Against the declared length: the file doesn't exist yet, and the real
       size is recorded once the upload lands. */
    let max_bytes_per_user = state.settings.read().await.max_bytes_per_user;
    if max_bytes_per_user > 0 {
        let used = storage::used_bytes(state, user_id).await?;
        if used + length > max_bytes_per_user as i64 {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "storage quota exceeded — free up space or ask the server admin",
            ));
        }
    }

    let now = Utc::now();
    let id = Uuid::new_v4().to_string();
    let image_key = storage_key(user_id, &format!("{}.{ext}", Uuid::new_v4()));

    sqlx::query(
        "INSERT INTO souvenirs
           (id, user_id, client_id, remote_game_id, image_key, status,
            achievement_names, captured_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'pending', '[]', ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(&client_id)
    .bind(request.remote_game_id)
    .bind(&image_key)
    .bind(now.timestamp_millis())
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .execute(&state.pool)
    .await?;

    Ok(Json(authorization(state, &image_key, limit)))
}

fn authorization(state: &AppState, image_key: &str, limit: NonZeroU64) -> Value {
    json!({
        "imageKey": image_key,
        "presignedUrl": storage::sign_upload_url(state, image_key, limit),
        "status": "pending",
        // Milliseconds — the launcher reads `expiresAt` as a JS timestamp.
        "expiresAt": (Utc::now().timestamp() + UPLOAD_TTL_SECONDS) * 1000,
    })
}

/// Called from [`crate::storage::finalize_upload`] once the bytes are on disk.
pub async fn mark_uploaded(state: &AppState, key: &str, written: u64) -> ApiResult<()> {
    sqlx::query(
        "UPDATE souvenirs SET is_uploaded = 1, size_in_bytes = ?, updated_at = ?
         WHERE image_key = ?",
    )
    .bind(written as i64)
    .bind(Utc::now().to_rfc3339())
    .bind(key)
    .execute(&state.pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Claiming a reservation from the achievement sync
// ---------------------------------------------------------------------------

/// One entry of the `souvenirs` array on `PUT /profile/games/achievements`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSouvenir {
    pub client_id: String,
    pub image_key: String,
    pub captured_at: i64,
    #[serde(default)]
    pub achievement_names: Vec<String>,
}

/// The game an achievement sync is about. `shop`/`object_id` are absent when
/// the launcher only knows the official game id.
#[derive(Clone, Copy)]
pub struct SyncGame<'a> {
    pub remote_id: &'a str,
    pub shop: Option<&'a str>,
    pub object_id: Option<&'a str>,
}

/// Promotes the souvenirs in an achievement sync and returns the
/// `[{ clientId, id }]` acknowledgements the launcher waits for.
///
/// `merged` is the achievement set the sync just stored, so a souvenir can only
/// be filed against unlocks this server knows about.
pub async fn claim_from_sync(
    state: &AppState,
    user_id: &str,
    game: SyncGame<'_>,
    souvenirs: &[SyncSouvenir],
    merged: &[Value],
) -> ApiResult<Vec<Value>> {
    let unlocked: HashSet<String> = merged
        .iter()
        .filter_map(|achievement| achievement.get("name")?.as_str())
        .map(str::to_uppercase)
        .collect();

    let mut acknowledgements = Vec::with_capacity(souvenirs.len());

    for souvenir in souvenirs {
        let client_id = souvenir.client_id.trim();
        if client_id.is_empty() {
            return Err(ApiError::bad_request("souvenir is missing a clientId"));
        }

        let id = claim_one(state, user_id, game, souvenir, client_id, &unlocked).await?;
        acknowledgements.push(json!({ "clientId": client_id, "id": id }));
    }

    Ok(acknowledgements)
}

/// Claims one reservation, returning the souvenir id to acknowledge.
async fn claim_one(
    state: &AppState,
    user_id: &str,
    game: SyncGame<'_>,
    souvenir: &SyncSouvenir,
    client_id: &str,
    unlocked: &HashSet<String>,
) -> ApiResult<String> {
    let mut names = normalize_names(&souvenir.achievement_names);
    if names.is_empty() || !key_belongs_to(user_id, &souvenir.image_key) {
        return Err(conflict("souvenir_payload_mismatch", client_id));
    }
    names.truncate(MAX_ACHIEVEMENTS_PER_SOUVENIR);

    /* "rebuild" is the launcher's recovery for this, which is what we want:
       its achievement state is ahead of ours. */
    if names.iter().any(|name| !unlocked.contains(name)) {
        return Err(conflict("achievement_not_found", client_id));
    }

    let reservation = sqlx::query(
        "SELECT id, image_key, is_uploaded FROM souvenirs
         WHERE user_id = ? AND client_id = ?",
    )
    .bind(user_id)
    .bind(client_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| conflict("reservation_not_found", client_id))?;

    let id: String = reservation.get("id");

    if reservation.get::<String, _>("image_key") != souvenir.image_key {
        return Err(conflict("reservation_mismatch", client_id));
    }

    /* The sync can overtake its own upload — a buffering proxy, an interrupted
       transfer — and the bytes are usually seconds away, so this asks for a
       retry rather than reporting a conflict. */
    if reservation.get::<i64, _>("is_uploaded") != 1 {
        return Err(ApiError::new(StatusCode::CONFLICT, UPLOAD_INCOMPLETE_CODE)
            .with_extra(json!({ "clientId": client_id })));
    }

    /* One souvenir per achievement, as the achievement list shows a single
       thumbnail per unlock. The launcher abandons a second capture and syncs
       the achievements alone. */
    if achievement_already_captured(state, user_id, game.remote_id, &id, &names).await? {
        return Err(conflict("achievement_already_assigned", client_id));
    }

    let names_json = serde_json::to_string(&names)
        .map_err(|_| ApiError::internal("failed to serialize achievement names"))?;
    let now = Utc::now().to_rfc3339();

    let size: i64 = sqlx::query_scalar(
        "UPDATE souvenirs SET
           status = 'ready',
           remote_game_id = ?,
           shop = COALESCE(?, shop),
           object_id = COALESCE(?, object_id),
           primary_achievement_name = ?,
           achievement_names = ?,
           captured_at = ?,
           updated_at = ?
         WHERE id = ?
         RETURNING size_in_bytes",
    )
    .bind(game.remote_id)
    .bind(game.shop)
    .bind(game.object_id)
    .bind(&names[0])
    .bind(&names_json)
    .bind(souvenir.captured_at)
    .bind(&now)
    .bind(&id)
    .fetch_one(&state.pool)
    .await?;

    let mut event = Event::sync(
        "souvenir.synced",
        user_id,
        match names.len() {
            1 => "Stored an achievement souvenir".to_string(),
            n => format!("Stored an achievement souvenir ({n} achievements)"),
        },
    )
    .detail(json!({ "souvenirId": id, "achievements": names }))
    .size(size);

    if let (Some(shop), Some(object_id)) = (game.shop, game.object_id) {
        event = event.game(shop, object_id);
    }

    crate::events::record(state, event).await;

    Ok(id)
}

/// Scoped to the game on purpose: achievement names are only unique within
/// one, and plenty of games ship an `ACH_WIN`.
async fn achievement_already_captured(
    state: &AppState,
    user_id: &str,
    remote_game_id: &str,
    souvenir_id: &str,
    names: &[String],
) -> ApiResult<bool> {
    let rows = sqlx::query(
        "SELECT achievement_names FROM souvenirs
         WHERE user_id = ? AND remote_game_id = ? AND status = 'ready' AND id <> ?",
    )
    .bind(user_id)
    .bind(remote_game_id)
    .bind(souvenir_id)
    .fetch_all(&state.pool)
    .await?;

    let taken: HashSet<String> = rows
        .iter()
        .flat_map(|row| parse_names(&row.get::<String, _>("achievement_names")))
        .collect();

    Ok(names.iter().any(|name| taken.contains(name)))
}

// ---------------------------------------------------------------------------
// Profile listing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    pub take: Option<i64>,
    pub skip: Option<i64>,
    pub sort_by: Option<String>,
}

/// Only `name` and `unlock_time` are ever known here; the launcher joins the
/// public catalogue for the display name, icon and points.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSouvenirAchievement {
    name: String,
    display_name: String,
    description: String,
    achievement_icon: Option<String>,
    unlock_time: i64,
    points: Option<i64>,
    is_rare: Option<bool>,
    is_platinum: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSouvenir {
    id: String,
    image_url: Option<String>,
    captured_at: i64,
    primary_achievement_name: String,
    achievements: Vec<ProfileSouvenirAchievement>,
    visibility: String,
    game_id: String,
    object_id: String,
    shop: String,
    game_title: Option<String>,
    game_icon_url: Option<String>,
    like_count: i64,
    liked_by_me: bool,
}

/// `shop` may be repeated (`?shop=steam&shop=launchbox`), which the typed
/// extractor collapses, so it is read off the raw query string.
fn shops_from_query(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else { return Vec::new() };

    raw.split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == "shop")
        .map(|(_, value)| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Lets the launcher say "hidden" rather than "none yet". `FRIENDS` reads as
/// "members of this server": the official friend graph isn't visible from
/// here, and everyone who can reach this server is someone the operator let in.
fn hidden_reason(account_visibility: &str, is_owner: bool) -> Option<&'static str> {
    if is_owner {
        return None;
    }

    match account_visibility {
        "PRIVATE" => Some("PRIVATE"),
        _ => None,
    }
}

/// GET /users/{userId}/souvenirs — the profile's souvenir tab.
///
/// Any member sees another's public souvenirs; the owner also sees the ones
/// they hid. An empty page tells the launcher to read the profile from
/// official Hydra instead, so a profile this server has never seen must not
/// come back claiming to be hidden; `isMember` says which of the two it was.
pub async fn list_for_user(
    State(state): State<AppState>,
    viewer: CurrentUser,
    Path(user_id): Path<String>,
    Query(query): Query<ListQuery>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let is_owner = viewer.0.id == user_id;

    let account_visibility: Option<String> = sqlx::query_scalar(
        "SELECT souvenirs_visibility FROM users WHERE id = ?",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    /* A profile this server has never seen keeps its souvenirs on official
       Hydra. Saying "hidden" would be a lie the launcher renders as a locked
       tab, so it is told plainly that this isn't the right server to ask. */
    let Some(account_visibility) = account_visibility else {
        return Ok(Json(json!({
            "items": [],
            "total": 0,
            "hiddenReason": Value::Null,
            "isMember": false,
        })));
    };

    if let Some(reason) = hidden_reason(&account_visibility, is_owner) {
        return Ok(Json(json!({
            "items": [],
            "total": 0,
            "hiddenReason": reason,
            "isMember": true,
        })));
    }

    let take = query.take.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let skip = query.skip.unwrap_or(0).max(0);
    let shops = shops_from_query(raw.as_deref());

    /* "rare" would rank by catalogue points, which never reach this server;
       capture order is the honest fallback. */
    let order = match query.sort_by.as_deref() {
        Some("oldest") => "s.captured_at ASC",
        _ => "s.captured_at DESC",
    };

    let mut filter = String::from(
        "FROM souvenirs s
         WHERE s.user_id = ? AND s.status = 'ready' AND s.is_uploaded = 1",
    );
    if !is_owner {
        filter.push_str(" AND s.visibility = 'PUBLIC'");
    }
    if !shops.is_empty() {
        filter.push_str(" AND s.shop IN (");
        filter.push_str(&vec!["?"; shops.len()].join(", "));
        filter.push(')');
    }

    let count_sql = format!("SELECT COUNT(*) {filter}");
    let mut count = sqlx::query(&count_sql).bind(&user_id);
    for shop in &shops {
        count = count.bind(shop);
    }
    let total: i64 = count.fetch_one(&state.pool).await?.get(0);

    // Binds in statement order: the sub-select's viewer id precedes the filter's.
    let page_sql = format!(
        "SELECT s.*,
                (SELECT COUNT(*) FROM souvenir_likes l WHERE l.souvenir_id = s.id) AS like_count,
                EXISTS(SELECT 1 FROM souvenir_likes l
                        WHERE l.souvenir_id = s.id AND l.user_id = ?) AS liked_by_me
         {filter}
         ORDER BY {order}
         LIMIT ? OFFSET ?"
    );
    let mut page = sqlx::query(&page_sql).bind(&viewer.0.id).bind(&user_id);
    for shop in &shops {
        page = page.bind(shop);
    }

    let rows = page
        .bind(take)
        .bind(skip)
        .fetch_all(&state.pool)
        .await?;

    let unlock_times = unlock_times_for(&state, &user_id, &rows).await?;
    let mut items = Vec::with_capacity(rows.len());

    for row in &rows {
        let shop: Option<String> = row.get("shop");
        let object_id: Option<String> = row.get("object_id");
        let remote_game_id: Option<String> = row.get("remote_game_id");
        let names = parse_names(&row.get::<String, _>("achievement_names"));
        let captured_at: i64 = row.get("captured_at");

        let metadata = match (shop.as_deref(), object_id.as_deref()) {
            (Some(shop), Some(object_id)) => games::resolve(&state, shop, object_id).await,
            _ => Default::default(),
        };

        let times = remote_game_id
            .as_deref()
            .and_then(|id| unlock_times.get(id));

        let primary: String = row
            .get::<Option<String>, _>("primary_achievement_name")
            .or_else(|| names.first().cloned())
            .unwrap_or_default();

        items.push(ProfileSouvenir {
            id: row.get("id"),
            image_url: Some(public_url(&state, &row.get::<String, _>("image_key"))),
            captured_at,
            primary_achievement_name: primary,
            achievements: names
                .into_iter()
                .map(|name| ProfileSouvenirAchievement {
                    unlock_time: times
                        .and_then(|times| times.get(&name).copied())
                        .unwrap_or(captured_at),
                    display_name: name.clone(),
                    name,
                    description: String::new(),
                    achievement_icon: None,
                    points: None,
                    is_rare: None,
                    is_platinum: false,
                })
                .collect(),
            visibility: row.get("visibility"),
            game_id: remote_game_id.unwrap_or_default(),
            object_id: object_id.unwrap_or_default(),
            shop: shop.unwrap_or_default(),
            game_title: metadata.name,
            game_icon_url: metadata.cover_url,
            like_count: row.get("like_count"),
            liked_by_me: row.get::<i64, _>("liked_by_me") == 1,
        });
    }

    Ok(Json(json!({
        "items": items,
        "total": total,
        "hiddenReason": Value::Null,
        "isMember": true,
    })))
}

/// So the profile shows when each achievement popped rather than when its
/// screenshot was written.
async fn unlock_times_for(
    state: &AppState,
    user_id: &str,
    rows: &[sqlx::sqlite::SqliteRow],
) -> ApiResult<HashMap<String, HashMap<String, i64>>> {
    let game_ids: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>("remote_game_id"))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if game_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; game_ids.len()].join(", ");
    let sql = format!(
        "SELECT remote_game_id, achievements FROM game_achievements
         WHERE user_id = ? AND remote_game_id IN ({placeholders})"
    );
    let mut query = sqlx::query(&sql).bind(user_id);

    for id in &game_ids {
        query = query.bind(id);
    }

    let mut times = HashMap::new();
    for row in query.fetch_all(&state.pool).await? {
        let achievements: Vec<Value> =
            serde_json::from_str(&row.get::<String, _>("achievements")).unwrap_or_default();

        times.insert(
            row.get::<String, _>("remote_game_id"),
            achievements
                .iter()
                .filter_map(|achievement| {
                    let name = achievement.get("name")?.as_str()?.to_uppercase();
                    let time = achievement
                        .get("unlockTime")
                        .or_else(|| achievement.get("unlockedAt"))?
                        .as_i64()?;
                    Some((name, time))
                })
                .collect::<HashMap<String, i64>>(),
        );
    }

    Ok(times)
}

// ---------------------------------------------------------------------------
// Per-achievement souvenir images
// ---------------------------------------------------------------------------

/// GET /users/{userId}/games/achievements — thumbnails for the achievement list.
///
/// Upstream serves these off the endpoint the launcher already used for a
/// profile's achievements, so the reply keeps the `UserAchievement` shape and
/// only `imageUrl` comes from here.
pub async fn user_game_achievements(
    State(state): State<AppState>,
    viewer: CurrentUser,
    Path(user_id): Path<String>,
    Query(query): Query<GameAchievementsQuery>,
) -> ApiResult<Json<Value>> {
    let shop = query.shop.unwrap_or_default();
    let object_id = query.object_id.unwrap_or_default();
    if shop.is_empty() || object_id.is_empty() {
        return Err(ApiError::bad_request("shop and objectId are required"));
    }

    let is_owner = viewer.0.id == user_id;

    let row = sqlx::query(
        "SELECT remote_game_id, achievements FROM game_achievements
         WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(&user_id)
    .bind(&shop)
    .bind(&object_id)
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = row else {
        return Ok(Json(json!([])));
    };

    let achievements: Vec<Value> =
        serde_json::from_str(&row.get::<String, _>("achievements")).unwrap_or_default();

    let mut images = sqlx::query(
        "SELECT achievement_names, image_key, visibility FROM souvenirs
         WHERE user_id = ? AND shop = ? AND object_id = ?
           AND status = 'ready' AND is_uploaded = 1
         ORDER BY captured_at ASC",
    )
    .bind(&user_id)
    .bind(&shop)
    .bind(&object_id)
    .fetch_all(&state.pool)
    .await?;

    if !is_owner {
        images.retain(|row| row.get::<String, _>("visibility") == "PUBLIC");
    }

    let mut image_by_name: HashMap<String, String> = HashMap::new();
    for image in &images {
        let key: String = image.get("image_key");
        for name in parse_names(&image.get::<String, _>("achievement_names")) {
            image_by_name.insert(name, public_url(&state, &key));
        }
    }

    let items: Vec<Value> = achievements
        .iter()
        .filter_map(|achievement| {
            let name = achievement.get("name")?.as_str()?.to_string();
            let image = image_by_name.get(&name.to_uppercase());

            Some(json!({
                "name": name,
                "unlocked": true,
                "unlockTime": achievement
                    .get("unlockTime")
                    .or_else(|| achievement.get("unlockedAt"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "imageUrl": image,
            }))
        })
        .collect();

    Ok(Json(json!(items)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAchievementsQuery {
    pub shop: Option<String>,
    pub object_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Likes, reports, visibility, deletion
// ---------------------------------------------------------------------------

async fn readable_souvenir(
    state: &AppState,
    owner_id: &str,
    souvenir_id: &str,
    viewer_id: &str,
) -> ApiResult<sqlx::sqlite::SqliteRow> {
    let row = sqlx::query(
        "SELECT id, user_id, visibility, image_key FROM souvenirs
         WHERE id = ? AND user_id = ? AND status = 'ready' AND is_uploaded = 1",
    )
    .bind(souvenir_id)
    .bind(owner_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("souvenir not found"))?;

    if owner_id != viewer_id && row.get::<String, _>("visibility") != "PUBLIC" {
        return Err(ApiError::not_found("souvenir not found"));
    }

    Ok(row)
}

/// POST /users/{userId}/souvenirs/{souvenirId}/like
///
/// A toggle, not an idempotent "like": the launcher flips its own state
/// optimistically and sends one POST for both directions.
pub async fn like(
    State(state): State<AppState>,
    viewer: CurrentUser,
    Path((user_id, souvenir_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    readable_souvenir(&state, &user_id, &souvenir_id, &viewer.0.id).await?;

    let deleted = sqlx::query("DELETE FROM souvenir_likes WHERE souvenir_id = ? AND user_id = ?")
        .bind(&souvenir_id)
        .bind(&viewer.0.id)
        .execute(&state.pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        sqlx::query(
            "INSERT INTO souvenir_likes (souvenir_id, user_id, created_at) VALUES (?, ?, ?)",
        )
        .bind(&souvenir_id)
        .bind(&viewer.0.id)
        .bind(Utc::now().to_rfc3339())
        .execute(&state.pool)
        .await?;
    }

    let like_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM souvenir_likes WHERE souvenir_id = ?")
            .bind(&souvenir_id)
            .fetch_one(&state.pool)
            .await?;

    Ok(Json(json!({
        "likeCount": like_count,
        "likedByMe": deleted == 0,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequest {
    pub reason: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// POST /users/{userId}/souvenirs/{souvenirId}/report
///
/// Raises a warning in the event log, which is what an operator watches and
/// what outlives the souvenir being deleted. 201 is the only status the
/// launcher reads as "reported", duplicates included.
pub async fn report(
    State(state): State<AppState>,
    viewer: CurrentUser,
    Path((user_id, souvenir_id)): Path<(String, String)>,
    Json(payload): Json<ReportRequest>,
) -> ApiResult<StatusCode> {
    let reason = payload.reason.trim().to_lowercase();
    if !ALLOWED_REPORT_REASONS.contains(&reason.as_str()) {
        return Err(ApiError::bad_request("unknown report reason"));
    }

    if viewer.0.id == user_id {
        return Err(ApiError::bad_request("cannot report your own souvenir"));
    }

    readable_souvenir(&state, &user_id, &souvenir_id, &viewer.0.id).await?;

    let since = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM souvenir_reports WHERE reporter_user_id = ? AND created_at > ?",
    )
    .bind(&viewer.0.id)
    .bind(&since)
    .fetch_one(&state.pool)
    .await?;

    if recent >= REPORT_RATE_LIMIT_PER_HOUR {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many reports — try again later",
        ));
    }

    let description = payload
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| description.chars().take(1000).collect::<String>());

    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO souvenir_reports
           (souvenir_id, owner_user_id, reporter_user_id, reason, description, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&souvenir_id)
    .bind(&user_id)
    .bind(&viewer.0.id)
    .bind(&reason)
    .bind(&description)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?
    .rows_affected();

    if inserted > 0 {
        crate::events::record(
            &state,
            Event::sync(
                "souvenir.reported",
                &user_id,
                format!("A souvenir was reported ({reason})"),
            )
            .actor(format!("user:{}", viewer.0.id))
            .detail(json!({
                "souvenirId": souvenir_id,
                "reason": reason,
                "description": description,
                "reporterUserId": viewer.0.id,
            }))
            .warning(),
        )
        .await;
    }

    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibilityRequest {
    pub visibility: String,
}

/// PATCH /profile/souvenirs/{souvenirId}/visibility — hide or show one picture.
pub async fn set_visibility(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(souvenir_id): Path<String>,
    Json(payload): Json<VisibilityRequest>,
) -> ApiResult<Json<Value>> {
    let visibility = match normalize_visibility(&payload.visibility) {
        Some(visibility @ ("PUBLIC" | "PRIVATE")) => visibility,
        _ => return Err(ApiError::bad_request("visibility must be PUBLIC or PRIVATE")),
    };

    let updated = sqlx::query(
        "UPDATE souvenirs SET visibility = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(visibility)
    .bind(Utc::now().to_rfc3339())
    .bind(&souvenir_id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?
    .rows_affected();

    if updated == 0 {
        return Err(ApiError::not_found("souvenir not found"));
    }

    Ok(Json(json!({ "ok": true, "visibility": visibility })))
}

/// PATCH /profile/souvenirs-visibility — the account-level setting.
///
/// The official profile owns this; the launcher mirrors it here because this
/// server answers for other viewers and can't read the official profile of
/// anyone but the caller.
pub async fn set_account_visibility(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<VisibilityRequest>,
) -> ApiResult<Json<Value>> {
    let visibility = normalize_visibility(&payload.visibility)
        .ok_or_else(|| ApiError::bad_request("unknown visibility"))?;

    sqlx::query("UPDATE users SET souvenirs_visibility = ? WHERE id = ?")
        .bind(visibility)
        .bind(&user.0.id)
        .execute(&state.pool)
        .await?;

    Ok(Json(json!({ "ok": true, "souvenirsVisibility": visibility })))
}

/// DELETE /profile/souvenirs/{souvenirId}
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(souvenir_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let freed = delete_owned(&state, &user.0.id, &souvenir_id).await?;

    Ok(Json(json!({ "ok": true, "freedBytes": freed })))
}

/// Shared with the portal, so ownership is re-checked here rather than at
/// either call site. Returns the bytes freed.
pub async fn delete_owned(
    state: &AppState,
    user_id: &str,
    souvenir_id: &str,
) -> ApiResult<i64> {
    let row = sqlx::query(
        "SELECT image_key, size_in_bytes FROM souvenirs WHERE id = ? AND user_id = ?",
    )
    .bind(souvenir_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("souvenir not found"))?;

    sqlx::query("DELETE FROM souvenirs WHERE id = ? AND user_id = ?")
        .bind(souvenir_id)
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    let key: String = row.get("image_key");
    storage::delete_object(state, &key).await;

    let freed: i64 = row.get("size_in_bytes");

    crate::events::record(
        state,
        Event::sync("souvenir.deleted", user_id, "Deleted an achievement souvenir")
            .detail(json!({ "souvenirId": souvenir_id }))
            .size(freed),
    )
    .await;

    Ok(freed)
}

// ---------------------------------------------------------------------------
// Housekeeping used by the admin panel
// ---------------------------------------------------------------------------

/// Read before the rows go: the database cascades, disk does not.
pub async fn storage_keys_for_user(state: &AppState, user_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT image_key FROM souvenirs WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
}

pub async fn purge_for_user(state: &AppState, user_id: &str) -> ApiResult<()> {
    let keys = storage_keys_for_user(state, user_id).await;

    sqlx::query("DELETE FROM souvenirs WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    // Not reachable from their own rows, so the cascade above misses these.
    sqlx::query("DELETE FROM souvenir_likes WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    for key in keys {
        storage::delete_object(state, &key).await;
    }

    Ok(())
}

/// Reservations older than `cutoff` (RFC 3339) whose upload never arrived. The
/// launcher rotates its client id rather than resuming, so nothing will ever
/// claim them.
pub async fn sweep_abandoned(state: &AppState, cutoff: &str) -> ApiResult<usize> {
    let stale = sqlx::query(
        "SELECT id, image_key FROM souvenirs
         WHERE status = 'pending' AND created_at < ?",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    for row in &stale {
        sqlx::query("DELETE FROM souvenirs WHERE id = ?")
            .bind(row.get::<String, _>("id"))
            .execute(&state.pool)
            .await?;

        storage::delete_object(state, &row.get::<String, _>("image_key")).await;
    }

    Ok(stale.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_keys_are_scoped_to_their_owner() {
        assert!(key_belongs_to("u1", "images/souvenirs/u1/a.jpg"));
        assert!(!key_belongs_to("u1", "images/souvenirs/u2/a.jpg"));
        assert!(!key_belongs_to("u1", "images/banners/u1/a.jpg"));
        assert!(!key_belongs_to("u1", "images/souvenirs/u1/../u2/a.jpg"));
    }

    #[test]
    fn achievement_names_are_upper_cased_and_deduplicated() {
        let names = normalize_names(&[
            "ach_win".into(),
            "ACH_WIN".into(),
            "  ".into(),
            "ach_lose".into(),
        ]);

        assert_eq!(names, vec!["ACH_WIN", "ACH_LOSE"]);
    }

    #[test]
    fn repeated_shop_parameters_are_all_read() {
        assert_eq!(
            shops_from_query(Some("take=24&shop=steam&shop=launchbox&sortBy=recent")),
            vec!["steam", "launchbox"]
        );
        assert!(shops_from_query(Some("take=24")).is_empty());
        assert!(shops_from_query(None).is_empty());
    }

    #[test]
    fn a_private_account_hides_its_souvenirs_from_others_only() {
        assert_eq!(hidden_reason("PRIVATE", false), Some("PRIVATE"));
        assert_eq!(hidden_reason("PRIVATE", true), None);
        assert_eq!(hidden_reason("PUBLIC", false), None);
        /* Friends-only means "members of this server" here, and a viewer that
           got this far is one. */
        assert_eq!(hidden_reason("FRIENDS", false), None);
    }

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("expected an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// A renamed or missing field doesn't fail loudly, it renders an empty
    /// card, so the shape the profile tab reads is asserted here.
    #[test]
    fn a_listed_souvenir_matches_the_launcher_shape() {
        let souvenir = ProfileSouvenir {
            id: "s1".into(),
            image_url: Some("http://example.test/images/souvenirs/u1/a.jpg".into()),
            captured_at: 1_700_000_000_000,
            primary_achievement_name: "ACH_WIN".into(),
            achievements: vec![ProfileSouvenirAchievement {
                name: "ACH_WIN".into(),
                display_name: "ACH_WIN".into(),
                description: String::new(),
                achievement_icon: None,
                unlock_time: 1_700_000_000_000,
                points: None,
                is_rare: None,
                is_platinum: false,
            }],
            visibility: "PUBLIC".into(),
            game_id: "remote-1".into(),
            object_id: "440".into(),
            shop: "steam".into(),
            game_title: Some("Team Fortress 2".into()),
            game_icon_url: None,
            like_count: 2,
            liked_by_me: true,
        };

        let value = serde_json::to_value(&souvenir).unwrap();

        assert_eq!(
            keys(&value),
            vec![
                "achievements",
                "capturedAt",
                "gameIconUrl",
                "gameId",
                "gameTitle",
                "id",
                "imageUrl",
                "likeCount",
                "likedByMe",
                "objectId",
                "primaryAchievementName",
                "shop",
                "visibility",
            ]
        );
        assert_eq!(
            keys(&value["achievements"][0]),
            vec![
                "achievementIcon",
                "description",
                "displayName",
                "isPlatinum",
                "isRare",
                "name",
                "points",
                "unlockTime",
            ]
        );
    }

    /// Truncated rather than refused: rejecting makes the launcher rotate its
    /// client id and re-upload the screenshot, on a loop.
    #[test]
    fn an_over_long_achievement_list_is_truncated_not_refused() {
        let names: Vec<String> = (0..=MAX_ACHIEVEMENTS_PER_SOUVENIR)
            .map(|index| format!("ACH_{index}"))
            .collect();

        let mut normalized = normalize_names(&names);
        assert_eq!(normalized.len(), MAX_ACHIEVEMENTS_PER_SOUVENIR + 1);

        normalized.truncate(MAX_ACHIEVEMENTS_PER_SOUVENIR);
        assert_eq!(normalized.len(), MAX_ACHIEVEMENTS_PER_SOUVENIR);
        assert_eq!(MAX_ACHIEVEMENTS_PER_SOUVENIR, 50);
    }

    /// Has to read as "retry": the launcher abandons a souvenir it believes
    /// the server refused outright.
    #[test]
    fn an_incomplete_upload_is_reported_with_its_own_code() {
        let error = ApiError::new(StatusCode::CONFLICT, UPLOAD_INCOMPLETE_CODE)
            .with_extra(json!({ "clientId": "client-1" }));

        assert_eq!(error.message, "achievements/souvenir-upload-incomplete");
        assert_eq!(error.extra.unwrap()["clientId"], "client-1");
    }

    #[test]
    fn conflicts_carry_the_code_and_reason_the_launcher_matches_on() {
        let error = conflict("reservation_not_found", "client-1");

        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.message, "achievements/souvenir-conflict");
        assert_eq!(
            error.extra.unwrap(),
            json!({ "reason": "reservation_not_found", "clientId": "client-1" })
        );
    }
}
