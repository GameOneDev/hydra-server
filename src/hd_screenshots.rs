//! 1080p copies of screenshots, for accounts that keep their cloud storage here.
//!
//! Official Hydra owns the screenshot itself — the record, its likes, comments
//! and moderation — and stores a 720p image for an account with no
//! subscription, 1080p for a subscriber. That 1080p tier is part of Hydra
//! Cloud, so an account that has this server instead of a subscription can't
//! get it there.
//!
//! What this server can do is keep the sharper copy. After the launcher has
//! uploaded the 720p screenshot to official Hydra it uploads a 1080p one here,
//! under the official screenshot's id, and when it later shows a screenshot it
//! asks this server whether a copy exists and prefers it. Where none does
//! (a subscriber's screenshot, a friend on another server, an upload that
//! never got here) it shows official's image, so nothing depends on this
//! feature having worked.
//!
//! A row is therefore never a screenshot of its own. It is an image attached
//! to an official screenshot id, and it is only ever shown in place of the
//! image of an official screenshot the launcher was already given. That is
//! what keeps this out of official's moderation and visibility decisions: a
//! screenshot official withheld is never listed, so its copy is never asked
//! for.
//!
//! Who may write a row is the uploader alone: the key is the owner plus the
//! official id, and a lookup asks for both, so one member can't plant a
//! picture under another member's screenshot.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::Row;
use uuid::Uuid;

/// The launcher's 1080p tier is capped at 5 MiB. Twice that leaves room for
/// another encoder without letting one screenshot take a share of the quota.
const MAX_IMAGE_BYTES: i64 = 10 * 1024 * 1024;

/// Far above any screen, low enough that a bogus size can't bloat a row.
const MAX_DIMENSION: i64 = 16_384;

/// One screenshot page is 24 items; the launcher asks for as many as it shows.
const MAX_LOOKUP: usize = 100;

const ALLOWED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// What the launcher's retry logic can tell apart from a failure to try again.
const UPLOAD_INCOMPLETE_CODE: &str = "screenshots-hd/upload-incomplete";
const NOT_FOUND_CODE: &str = "screenshots-hd/not-found";

/// Official ids are UUIDs; anything longer or odder is not one.
fn valid_screenshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn normalize_visibility(value: &str) -> Option<&'static str> {
    match value.to_uppercase().as_str() {
        "PUBLIC" => Some("PUBLIC"),
        "PRIVATE" => Some("PRIVATE"),
        "FRIENDS" => Some("FRIENDS"),
        _ => None,
    }
}

fn storage_key(user_id: &str, file_name: &str) -> String {
    format!("images/screenshots-hd/{user_id}/{file_name}")
}

fn public_url(state: &AppState, key: &str) -> String {
    format!(
        "{}/{}",
        state.config.public_url,
        key.trim_start_matches('/')
    )
}

/// The storage key behind `image_url`, if this server issued it to `user_id`.
/// Otherwise a client could claim someone else's stored file.
fn key_issued_to(state: &AppState, user_id: &str, image_url: &str) -> Option<String> {
    let prefix = format!(
        "{}/images/screenshots-hd/{user_id}/",
        state.config.public_url
    );
    let file_name = image_url.strip_prefix(&prefix)?;

    let safe = !file_name.is_empty()
        && !file_name.contains("..")
        && file_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));

    safe.then(|| storage_key(user_id, file_name))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadUrlRequest {
    pub image_ext: String,
    #[serde(default)]
    pub image_length: Option<i64>,
}

/// POST /profile/hd-screenshots/{screenshotId}/upload-url
///
/// Hands back a presigned PUT and the public URL the image will live at, the
/// same two-step shape as custom artwork: the launcher uploads to the first,
/// then records the second with the PUT below.
pub async fn upload_url(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(screenshot_id): Path<String>,
    Json(payload): Json<UploadUrlRequest>,
) -> ApiResult<Json<Value>> {
    if !valid_screenshot_id(&screenshot_id) {
        return Err(ApiError::bad_request("invalid screenshot id"));
    }

    let ext = payload.image_ext.trim_start_matches('.').to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ApiError::bad_request("unsupported image format"));
    }

    /* The declared size is what the quota is checked against and what the
    signed URL is bound to; `storage::upload` then holds it to the real bytes,
    so understating it buys nothing. */
    let length = payload.image_length.unwrap_or(0);
    let limit = storage::upload_limit(length)
        .ok_or_else(|| ApiError::bad_request("imageLength is required"))?;

    if length > MAX_IMAGE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "image is too large",
        ));
    }

    storage::check_quota(&state, &user.0.id, length).await?;

    /* Flat, uuid-named keys: which screenshot a file belongs to lives in the
    database, so nothing from the request ends up in a filesystem path. */
    let key = storage_key(&user.0.id, &format!("{}.{ext}", Uuid::new_v4()));

    Ok(Json(json!({
        "presignedUrl": storage::sign_upload_url(&state, &key, limit),
        "imageUrl": public_url(&state, &key),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRequest {
    pub image_url: String,
    pub width: i64,
    pub height: i64,
    pub visibility: String,
}

/// PUT /profile/hd-screenshots/{screenshotId}
///
/// Attaches the uploaded image to the official screenshot. Doing it again for
/// the same screenshot replaces the copy and deletes the file it superseded,
/// so a retry can't leave two behind.
pub async fn save(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(screenshot_id): Path<String>,
    Json(payload): Json<SaveRequest>,
) -> ApiResult<Json<Value>> {
    if !valid_screenshot_id(&screenshot_id) {
        return Err(ApiError::bad_request("invalid screenshot id"));
    }

    let visibility = normalize_visibility(&payload.visibility)
        .ok_or_else(|| ApiError::bad_request("invalid visibility"))?;

    let sane_size = |value: i64| (1..=MAX_DIMENSION).contains(&value);
    if !sane_size(payload.width) || !sane_size(payload.height) {
        return Err(ApiError::bad_request("invalid image size"));
    }

    let key = key_issued_to(&state, &user.0.id, &payload.image_url)
        .ok_or_else(|| ApiError::bad_request("image url was not issued by this server"))?;

    /* The size on disk, not the one the launcher predicted. A file that is not
    there means the PUT never landed, which is worth another attempt rather
    than recording a copy that would 404. */
    let size_in_bytes = match tokio::fs::metadata(storage::storage_path(&state, &key)).await {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => metadata.len() as i64,
        _ => {
            return Err(ApiError::new(StatusCode::CONFLICT, UPLOAD_INCOMPLETE_CODE));
        }
    };

    /* One file, one screenshot: the unique index would refuse it as a 500. */
    let claimed_elsewhere: Option<String> = sqlx::query_scalar(
        "SELECT screenshot_id FROM hd_screenshots
         WHERE storage_key = ? AND NOT (user_id = ? AND screenshot_id = ?)",
    )
    .bind(&key)
    .bind(&user.0.id)
    .bind(&screenshot_id)
    .fetch_optional(&state.pool)
    .await?;
    if claimed_elsewhere.is_some() {
        return Err(ApiError::bad_request(
            "image is already attached to another screenshot",
        ));
    }

    let previous: Option<String> = sqlx::query_scalar(
        "SELECT storage_key FROM hd_screenshots WHERE user_id = ? AND screenshot_id = ?",
    )
    .bind(&user.0.id)
    .bind(&screenshot_id)
    .fetch_optional(&state.pool)
    .await?;

    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO hd_screenshots
           (user_id, screenshot_id, storage_key, width, height, size_in_bytes,
            visibility, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id, screenshot_id) DO UPDATE SET
           storage_key = excluded.storage_key,
           width = excluded.width,
           height = excluded.height,
           size_in_bytes = excluded.size_in_bytes,
           visibility = excluded.visibility,
           updated_at = excluded.updated_at",
    )
    .bind(&user.0.id)
    .bind(&screenshot_id)
    .bind(&key)
    .bind(payload.width)
    .bind(payload.height)
    .bind(size_in_bytes)
    .bind(visibility)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    if let Some(previous) = previous {
        if previous != key {
            storage::delete_object(&state, &previous).await;
        }
    }

    Ok(Json(json!({ "ok": true, "url": public_url(&state, &key) })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisibilityRequest {
    pub visibility: String,
}

/// PATCH /profile/hd-screenshots/{screenshotId} — the owner changed who may
/// see the official screenshot, and this copy follows.
///
/// A 404 is not a failure for the launcher: not every screenshot has a copy
/// here, and it mirrors every visibility change without knowing which.
pub async fn set_visibility(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(screenshot_id): Path<String>,
    Json(payload): Json<VisibilityRequest>,
) -> ApiResult<Json<Value>> {
    if !valid_screenshot_id(&screenshot_id) {
        return Err(ApiError::bad_request("invalid screenshot id"));
    }

    let visibility = normalize_visibility(&payload.visibility)
        .ok_or_else(|| ApiError::bad_request("invalid visibility"))?;

    let updated = sqlx::query(
        "UPDATE hd_screenshots SET visibility = ?, updated_at = ?
         WHERE user_id = ? AND screenshot_id = ?",
    )
    .bind(visibility)
    .bind(Utc::now().to_rfc3339())
    .bind(&user.0.id)
    .bind(&screenshot_id)
    .execute(&state.pool)
    .await?;

    if updated.rows_affected() == 0 {
        return Err(ApiError::not_found(NOT_FOUND_CODE));
    }

    Ok(Json(json!({ "ok": true })))
}

/// DELETE /profile/hd-screenshots/{screenshotId} — the screenshot was deleted
/// on official Hydra, or its copy here is no longer wanted.
///
/// Succeeds when there is nothing to delete, for the same reason the PATCH
/// above treats a missing row as ordinary.
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(screenshot_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !valid_screenshot_id(&screenshot_id) {
        return Err(ApiError::bad_request("invalid screenshot id"));
    }

    let removed: Option<String> = sqlx::query_scalar(
        "DELETE FROM hd_screenshots WHERE user_id = ? AND screenshot_id = ?
         RETURNING storage_key",
    )
    .bind(&user.0.id)
    .bind(&screenshot_id)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(key) = removed {
        storage::delete_object(&state, &key).await;
    }

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupRequest {
    #[serde(default)]
    pub screenshots: Vec<LookupRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupRef {
    pub user_id: String,
    pub id: String,
}

/// POST /profile/hd-screenshots/lookup
///
/// Which of these official screenshots have a 1080p copy here. Each entry names
/// the screenshot's *owner* as well as its id, because that is what the row is
/// keyed by: the answer for (alice, X) is alice's copy of X and nothing a
/// different member attached to the same id.
///
/// A POST because the list can be a page long, not because it changes anything.
///
/// The answer is a map keyed by screenshot id holding only the ones found, so
/// "no entry" is the ordinary, cheap way for the launcher to learn it should
/// keep official's image. The owner sees every copy; other members see all but
/// the private ones.
pub async fn lookup(
    State(state): State<AppState>,
    viewer: CurrentUser,
    Json(payload): Json<LookupRequest>,
) -> ApiResult<Json<Value>> {
    if payload.screenshots.len() > MAX_LOOKUP {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_LOOKUP} screenshots per lookup"
        )));
    }

    let mut items = Map::new();

    for reference in &payload.screenshots {
        if !valid_screenshot_id(&reference.id) || reference.user_id.is_empty() {
            continue;
        }

        let row = sqlx::query(
            "SELECT storage_key, width, height, visibility, updated_at
             FROM hd_screenshots WHERE user_id = ? AND screenshot_id = ?",
        )
        .bind(&reference.user_id)
        .bind(&reference.id)
        .fetch_optional(&state.pool)
        .await?;

        let Some(row) = row else { continue };

        let is_owner = reference.user_id == viewer.0.id;
        if !is_owner && row.get::<String, _>("visibility") == "PRIVATE" {
            continue;
        }

        items.insert(
            reference.id.clone(),
            json!({
                "url": public_url(&state, &row.get::<String, _>("storage_key")),
                "width": row.get::<i64, _>("width"),
                "height": row.get::<i64, _>("height"),
                "updatedAt": row.get::<String, _>("updated_at"),
            }),
        );
    }

    Ok(Json(json!({ "items": items })))
}

/// Storage keys of every copy a user uploaded, so account deletion can remove
/// the files along with the rows.
pub async fn storage_keys_for_user(state: &AppState, user_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT storage_key FROM hd_screenshots WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
}

/// The panel's "purge HD screenshots": the rows and their files.
pub async fn purge_for_user(state: &AppState, user_id: &str) -> ApiResult<()> {
    let keys = storage_keys_for_user(state, user_id).await;

    sqlx::query("DELETE FROM hd_screenshots WHERE user_id = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    for key in keys {
        storage::delete_object(state, &key).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    /// What the storage PUT would have written, since the presigned route is
    /// exercised in `storage`'s own tests.
    async fn land(server: &TestServer, url: &str, bytes: &[u8]) {
        let key = url
            .strip_prefix(&format!("{}/", server.state.config.public_url))
            .expect("a url on this server");
        let path = storage::storage_path(&server.state, key);
        tokio::fs::create_dir_all(path.parent().expect("a directory"))
            .await
            .expect("the owner's directory");
        tokio::fs::write(path, bytes).await.expect("the image");
    }

    async fn reserve(server: &TestServer, user: &str, screenshot_id: &str, length: i64) -> String {
        let Json(response) = upload_url(
            State(server.state.clone()),
            server.user(user),
            Path(screenshot_id.to_string()),
            Json(UploadUrlRequest {
                image_ext: "jpg".to_string(),
                image_length: Some(length),
            }),
        )
        .await
        .expect("an upload url");

        response["imageUrl"]
            .as_str()
            .expect("an imageUrl")
            .to_string()
    }

    async fn attach(
        server: &TestServer,
        user: &str,
        screenshot_id: &str,
        image_url: &str,
        visibility: &str,
    ) -> ApiResult<Value> {
        save(
            State(server.state.clone()),
            server.user(user),
            Path(screenshot_id.to_string()),
            Json(SaveRequest {
                image_url: image_url.to_string(),
                width: 1920,
                height: 1080,
                visibility: visibility.to_string(),
            }),
        )
        .await
        .map(|Json(body)| body)
    }

    async fn find(server: &TestServer, viewer: &str, owner: &str, id: &str) -> Option<Value> {
        let Json(response) = lookup(
            State(server.state.clone()),
            server.user(viewer),
            Json(LookupRequest {
                screenshots: vec![LookupRef {
                    user_id: owner.to_string(),
                    id: id.to_string(),
                }],
            }),
        )
        .await
        .expect("a lookup");

        response["items"].get(id).cloned()
    }

    #[tokio::test]
    async fn an_upload_is_checked_before_a_url_is_signed() {
        let server = TestServer::start().await;
        let ask = |id: &str, ext: &str, length: Option<i64>| {
            upload_url(
                State(server.state.clone()),
                server.user("alice"),
                Path(id.to_string()),
                Json(UploadUrlRequest {
                    image_ext: ext.to_string(),
                    image_length: length,
                }),
            )
        };

        let status = |result: ApiResult<Json<Value>>| result.expect_err("a refusal").status;

        assert_eq!(
            status(ask("../../etc", "jpg", Some(10)).await),
            StatusCode::BAD_REQUEST,
            "an id that is not an official one"
        );
        assert_eq!(
            status(ask("shot-1", "exe", Some(10)).await),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(ask("shot-1", "jpg", None).await),
            StatusCode::BAD_REQUEST,
            "no declared size to bind the URL to"
        );
        assert_eq!(
            status(ask("shot-1", "jpg", Some(MAX_IMAGE_BYTES + 1)).await),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let Json(issued) = ask("shot-1", ".JPG", Some(1_000_000))
            .await
            .expect("a valid request");
        assert!(issued["presignedUrl"].as_str().is_some());
        assert!(issued["imageUrl"]
            .as_str()
            .expect("a url")
            .contains("/images/screenshots-hd/alice/"));
    }

    #[tokio::test]
    async fn a_full_account_cannot_reserve_another_copy() {
        let server = TestServer::start().await;
        server
            .limits(
                "alice",
                crate::limits::Overrides {
                    max_bytes_per_user: Some(1_000_000),
                    ..Default::default()
                },
            )
            .await;

        let refusal = upload_url(
            State(server.state.clone()),
            server.user("alice"),
            Path("shot-1".to_string()),
            Json(UploadUrlRequest {
                image_ext: "jpg".to_string(),
                image_length: Some(2_000_000),
            }),
        )
        .await
        .expect_err("2 MB into a 1 MB quota");

        assert_eq!(refusal.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn an_uploaded_copy_is_found_and_counted_against_the_quota() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &url, &vec![9; 4_000]).await;

        let saved = attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("the copy");
        assert_eq!(saved["url"], url.as_str());

        let found = find(&server, "alice", "alice", "shot-1")
            .await
            .expect("alice's own copy");
        assert_eq!(found["url"], url.as_str());
        assert_eq!(found["width"], 1920);
        assert_eq!(found["height"], 1080);

        /* The real size on disk, which is what the quota is measured in. */
        assert_eq!(
            storage::used_bytes(&server.state, "alice")
                .await
                .expect("a total"),
            4_000
        );
    }

    #[tokio::test]
    async fn a_copy_whose_bytes_never_arrived_is_not_recorded() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 4_000).await;

        let refusal = attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect_err("nothing was uploaded");

        assert_eq!(refusal.status, StatusCode::CONFLICT);
        assert!(
            find(&server, "alice", "alice", "shot-1").await.is_none(),
            "a row here would answer with a URL that 404s"
        );
    }

    #[tokio::test]
    async fn a_url_this_server_issued_to_someone_else_is_refused() {
        let server = TestServer::start().await;
        server.add_user("bob").await;

        let url = reserve(&server, "bob", "shot-1", 4_000).await;
        land(&server, &url, &[1; 4_000]).await;

        let refusal = attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect_err("bob's file");

        assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn one_file_cannot_stand_for_two_screenshots() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &url, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("the first");

        let refusal = attach(&server, "alice", "shot-2", &url, "PUBLIC")
            .await
            .expect_err("the same file for another screenshot");

        assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    }

    /// The row belongs to whoever uploaded it, so planting a picture under
    /// someone else's screenshot id changes nothing about what their copy says.
    #[tokio::test]
    async fn a_member_cannot_plant_a_copy_under_another_members_screenshot() {
        let server = TestServer::start().await;
        server.add_user("bob").await;

        let real = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &real, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &real, "PUBLIC")
            .await
            .expect("alice's copy");

        let forged = reserve(&server, "bob", "shot-1", 4_000).await;
        land(&server, &forged, &[2; 4_000]).await;
        attach(&server, "bob", "shot-1", &forged, "PUBLIC")
            .await
            .expect("bob's own row, under the same id");

        let found = find(&server, "bob", "alice", "shot-1")
            .await
            .expect("alice's copy");
        assert_eq!(found["url"], real.as_str());

        /* And a screenshot nobody attached a copy to has none to find. */
        assert!(find(&server, "bob", "alice", "shot-9").await.is_none());
    }

    #[tokio::test]
    async fn a_private_copy_is_only_handed_to_its_owner() {
        let server = TestServer::start().await;
        server.add_user("bob").await;

        let url = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &url, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &url, "private")
            .await
            .expect("a private copy");

        assert!(find(&server, "alice", "alice", "shot-1").await.is_some());
        assert!(
            find(&server, "bob", "alice", "shot-1").await.is_none(),
            "private on official Hydra stays private here"
        );

        /* Following the official screenshot when its owner opens it up. */
        set_visibility(
            State(server.state.clone()),
            server.user("alice"),
            Path("shot-1".to_string()),
            Json(VisibilityRequest {
                visibility: "FRIENDS".to_string(),
            }),
        )
        .await
        .map(|_| ())
        .expect("a visibility change");
        assert!(find(&server, "bob", "alice", "shot-1").await.is_some());
    }

    #[tokio::test]
    async fn changing_the_visibility_of_a_screenshot_with_no_copy_is_a_404() {
        let server = TestServer::start().await;

        let missing = set_visibility(
            State(server.state.clone()),
            server.user("alice"),
            Path("shot-1".to_string()),
            Json(VisibilityRequest {
                visibility: "PUBLIC".to_string(),
            }),
        )
        .await
        .expect_err("no copy to change");

        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn replacing_a_copy_deletes_the_file_it_superseded() {
        let server = TestServer::start().await;
        let first = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &first, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &first, "PUBLIC")
            .await
            .expect("the first copy");

        let second = reserve(&server, "alice", "shot-1", 5_000).await;
        land(&server, &second, &[2; 5_000]).await;
        attach(&server, "alice", "shot-1", &second, "PUBLIC")
            .await
            .expect("the replacement");

        let on_disk = |url: &str| {
            let key = url
                .strip_prefix(&format!("{}/", server.state.config.public_url))
                .expect("a url on this server");
            storage::storage_path(&server.state, key).exists()
        };

        assert!(!on_disk(&first), "the superseded file is gone");
        assert!(on_disk(&second));
        assert_eq!(
            server
                .scalar::<i64>("SELECT COUNT(*) FROM hd_screenshots")
                .await,
            1
        );
        assert_eq!(
            storage::used_bytes(&server.state, "alice")
                .await
                .expect("a total"),
            5_000
        );
    }

    #[tokio::test]
    async fn deleting_removes_the_row_and_the_file_and_is_repeatable() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &url, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("a copy");

        for _ in 0..2 {
            delete(
                State(server.state.clone()),
                server.user("alice"),
                Path("shot-1".to_string()),
            )
            .await
            .map(|_| ())
            .expect("a delete, whether or not there is anything left");
        }

        assert!(find(&server, "alice", "alice", "shot-1").await.is_none());
        assert_eq!(
            storage::used_bytes(&server.state, "alice")
                .await
                .expect("a total"),
            0
        );
        let key = url
            .strip_prefix(&format!("{}/", server.state.config.public_url))
            .expect("a url on this server");
        assert!(!storage::storage_path(&server.state, key).exists());
    }

    #[tokio::test]
    async fn a_lookup_is_bounded_and_skips_what_is_not_an_id() {
        let server = TestServer::start().await;

        let too_many = lookup(
            State(server.state.clone()),
            server.user("alice"),
            Json(LookupRequest {
                screenshots: (0..=MAX_LOOKUP)
                    .map(|n| LookupRef {
                        user_id: "alice".to_string(),
                        id: format!("shot-{n}"),
                    })
                    .collect(),
            }),
        )
        .await
        .expect_err("one over the cap");
        assert_eq!(too_many.status, StatusCode::BAD_REQUEST);

        let Json(skipped) = lookup(
            State(server.state.clone()),
            server.user("alice"),
            Json(LookupRequest {
                screenshots: vec![
                    LookupRef {
                        user_id: "alice".to_string(),
                        id: "../x".to_string(),
                    },
                    LookupRef {
                        user_id: String::new(),
                        id: "shot-1".to_string(),
                    },
                ],
            }),
        )
        .await
        .expect("malformed entries are skipped, not fatal");
        assert_eq!(skipped["items"], json!({}));
    }

    #[tokio::test]
    async fn purging_an_account_takes_its_rows_and_files() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 4_000).await;
        land(&server, &url, &[1; 4_000]).await;
        attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("a copy");

        assert_eq!(storage_keys_for_user(&server.state, "alice").await.len(), 1);

        purge_for_user(&server.state, "alice")
            .await
            .expect("a purge");

        assert!(storage_keys_for_user(&server.state, "alice")
            .await
            .is_empty());
        assert_eq!(
            server
                .scalar::<i64>("SELECT COUNT(*) FROM hd_screenshots")
                .await,
            0
        );
    }
}
