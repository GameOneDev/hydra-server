use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Matches the launcher's per-image cap for custom artwork.
const MAX_ARTWORK_BYTES: i64 = 20 * 1024 * 1024;

const ALLOWED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "apng", "gif", "webp"];

/// Artwork kinds as they appear in the launcher's request paths. The launcher
/// maps its own asset types onto these: grid -> grids (library cover), hero ->
/// heroes (banner), logo -> logos, icon -> icons.
const ALLOWED_KINDS: &[&str] = &["grids", "heroes", "logos", "icons"];

/// The launcher sends `upload` for a file the user picked off disk and
/// `steamgriddb` for an image chosen from SteamGridDB.
const ALLOWED_SOURCES: &[&str] = &["upload", "steamgriddb"];

fn validate_kind(kind: &str) -> ApiResult<()> {
    if ALLOWED_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(ApiError::not_found("unknown artwork kind"))
    }
}

/// Shop/object ids only ever reach the database, but keeping them short and
/// printable stops a malformed launcher from bloating rows.
fn validate_game(shop: &str, object_id: &str) -> ApiResult<()> {
    let sane = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
    };

    if sane(shop) && sane(object_id) {
        Ok(())
    } else {
        Err(ApiError::bad_request("invalid game reference"))
    }
}

/// Response shape the launcher merges into its library. `url` is always
/// directly loadable: either this server's `/images/...` path for uploads or
/// SteamGridDB's CDN for picks.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtworkEntry {
    pub shop: String,
    pub object_id: String,
    pub kind: String,
    pub source: String,
    pub url: String,
    pub updated_at: String,
}

fn entry_from_row(row: &sqlx::sqlite::SqliteRow) -> ArtworkEntry {
    ArtworkEntry {
        shop: row.get("shop"),
        object_id: row.get("object_id"),
        kind: row.get("kind"),
        source: row.get("source"),
        url: row.get("url"),
        updated_at: row.get("updated_at"),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadUrlRequest {
    pub image_ext: String,
    #[serde(default)]
    pub image_length: Option<i64>,
}

/// POST /profile/games/{shop}/{objectId}/artwork/{kind}/upload-url
///
/// Mirrors the official Hydra Cloud endpoint: hands back a presigned PUT URL
/// plus the public URL the image will live at. The launcher uploads to the
/// former, then PUTs the latter to the endpoint below to record the choice.
pub async fn upload_url(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((shop, object_id, kind)): Path<(String, String, String)>,
    Json(payload): Json<UploadUrlRequest>,
) -> ApiResult<Json<Value>> {
    validate_game(&shop, &object_id)?;
    validate_kind(&kind)?;

    let ext = payload.image_ext.trim_start_matches('.').to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ApiError::bad_request("unsupported image format"));
    }

    let length = payload.image_length.unwrap_or(0);
    if length > MAX_ARTWORK_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "image is too large",
        ));
    }

    /* Checked against the length the launcher declares, since the file
       doesn't exist yet. The stored size recorded in `save` is the real one,
       so an understated length can overshoot the quota by at most one
       image. */
    let max_bytes_per_user = state.settings.read().await.max_bytes_per_user;
    if max_bytes_per_user > 0 {
        let used = storage::used_bytes(&state, &user.0.id).await?;

        if used + length.max(0) > max_bytes_per_user as i64 {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "storage quota exceeded — free up space or ask the server admin",
            ));
        }
    }

    /* Flat, uuid-named keys: the game a file belongs to is tracked in the
       database, so nothing user-controlled ends up in a filesystem path. */
    let file_name = format!("{}.{ext}", Uuid::new_v4());
    let key = format!("images/artwork/{}/{file_name}", user.0.id);

    Ok(Json(json!({
        "presignedUrl": storage::sign_upload_url(&state, &key, length.max(0) as u64),
        "imageUrl": format!(
            "{}/images/artwork/{}/{file_name}",
            state.config.public_url, user.0.id
        ),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveArtworkRequest {
    pub source: String,
    pub url: String,
}

/// PUT /profile/games/{shop}/{objectId}/artwork/{kind}
///
/// Records the image the user picked for a game. Replacing an uploaded image
/// deletes the file it superseded so abandoned uploads don't accumulate.
pub async fn save(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((shop, object_id, kind)): Path<(String, String, String)>,
    Json(payload): Json<SaveArtworkRequest>,
) -> ApiResult<Json<Value>> {
    validate_game(&shop, &object_id)?;
    validate_kind(&kind)?;

    if !ALLOWED_SOURCES.contains(&payload.source.as_str()) {
        return Err(ApiError::bad_request("unknown artwork source"));
    }

    if payload.url.len() > 2048 || !payload.url.starts_with("http") {
        return Err(ApiError::bad_request("invalid artwork url"));
    }

    /* Uploads must point at a key this server just signed for this user;
       otherwise a client could claim someone else's stored file. */
    let storage_key = if payload.source == "upload" {
        let prefix = format!("{}/images/artwork/{}/", state.config.public_url, user.0.id);
        let file_name = payload
            .url
            .strip_prefix(&prefix)
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| ApiError::bad_request("artwork url was not issued by this server"))?;

        Some(format!("images/artwork/{}/{file_name}", user.0.id))
    } else {
        None
    };

    /* The upload has landed by now, so this is the real size on disk rather
       than the length the launcher predicted. SteamGridDB picks stay at 0 —
       they cost this server nothing. */
    let size_in_bytes = match &storage_key {
        Some(key) => tokio::fs::metadata(storage::storage_path(&state, key))
            .await
            .map(|metadata| metadata.len() as i64)
            .unwrap_or(0),
        None => 0,
    };

    let previous: Option<String> = sqlx::query_scalar(
        "SELECT storage_key FROM game_artwork
         WHERE user_id = ? AND shop = ? AND object_id = ? AND kind = ?",
    )
    .bind(&user.0.id)
    .bind(&shop)
    .bind(&object_id)
    .bind(&kind)
    .fetch_optional(&state.pool)
    .await?
    .flatten();

    sqlx::query(
        "INSERT INTO game_artwork
           (user_id, shop, object_id, kind, source, url, storage_key, size_in_bytes, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, shop, object_id, kind) DO UPDATE SET
           source = excluded.source,
           url = excluded.url,
           storage_key = excluded.storage_key,
           size_in_bytes = excluded.size_in_bytes,
           updated_at = excluded.updated_at",
    )
    .bind(&user.0.id)
    .bind(&shop)
    .bind(&object_id)
    .bind(&kind)
    .bind(&payload.source)
    .bind(&payload.url)
    .bind(&storage_key)
    .bind(size_in_bytes)
    .bind(Utc::now().to_rfc3339())
    .execute(&state.pool)
    .await?;

    /* An upload that replaced another upload leaves the old file orphaned. */
    if let Some(key) = previous {
        if storage_key.as_deref() != Some(key.as_str()) {
            storage::delete_object(&state, &key).await;
        }
    }

    Ok(Json(json!({ "ok": true })))
}

/// DELETE /profile/games/{shop}/{objectId}/artwork/{kind} — the user reverted
/// to the shop's default image.
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((shop, object_id, kind)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    validate_game(&shop, &object_id)?;
    validate_kind(&kind)?;

    let removed: Option<Option<String>> = sqlx::query_scalar(
        "DELETE FROM game_artwork
         WHERE user_id = ? AND shop = ? AND object_id = ? AND kind = ?
         RETURNING storage_key",
    )
    .bind(&user.0.id)
    .bind(&shop)
    .bind(&object_id)
    .bind(&kind)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(key) = removed.flatten() {
        storage::delete_object(&state, &key).await;
    }

    Ok(Json(json!({ "ok": true })))
}

/// GET /profile/games/artwork — every custom image the caller has saved here,
/// so a freshly installed launcher can repaint its whole library at once.
pub async fn list(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "artwork": fetch_for_user(&state, &user.0.id).await? })))
}

/// GET /profile/games/artwork/{userId} — the same listing for someone else on
/// this server, so their custom images show on their profile. Hydra Cloud
/// makes these visible to anyone viewing the profile, so this matches: any
/// authenticated user of this server may read them.
pub async fn list_for_user(
    State(state): State<AppState>,
    _viewer: CurrentUser,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "artwork": fetch_for_user(&state, &user_id).await? })))
}

async fn fetch_for_user(state: &AppState, user_id: &str) -> ApiResult<Vec<ArtworkEntry>> {
    let rows = sqlx::query(
        "SELECT shop, object_id, kind, source, url, updated_at
         FROM game_artwork WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(rows.iter().map(entry_from_row).collect())
}

/// Storage keys of every artwork a user uploaded, so account deletion can
/// remove the files along with the rows.
pub async fn storage_keys_for_user(state: &AppState, user_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT storage_key FROM game_artwork WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .flatten()
    .collect()
}
