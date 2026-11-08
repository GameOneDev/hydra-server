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
use crate::events::{self, Event};
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, State};
use axum::http::{header::HOST, HeaderMap, StatusCode};
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

/// Text from a request, made safe to keep in the history: no control
/// characters, no more than `max` characters.
fn clip(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Why the address matters, for whoever is reading a failure: the launcher
/// does not reuse the address it reached the server on.
const PUBLIC_URL_WHY: &str = "When a launcher uploads an image, this server tells it where to \
    send it: HYDRA_SERVER_PUBLIC_URL followed by /storage/\u{2026}. The launcher connects to exactly \
    that address, not to the one it used to reach this server, so it has to work from the \
    launcher's own computer. (There, localhost means the launcher itself, not this server.)";

/// `text` cut to `max` characters, with an ellipsis when it was cut.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }

    text.chars().take(max.saturating_sub(1)).collect::<String>() + "\u{2026}"
}

/// `scheme://host:port` of this server's public address: where the launcher is
/// told to send the image, without the token in the rest of the URL.
fn storage_origin(state: &AppState) -> String {
    clip(&state.config.public_url, 200)
}

fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers.get(HOST).and_then(|value| value.to_str().ok())
}

/// Writes a refused upload to the history, then hands the error back so the
/// call site stays a single `return Err(...)`.
async fn refused(
    state: &AppState,
    user_id: &str,
    screenshot_id: &str,
    why: &str,
    error: ApiError,
) -> ApiError {
    events::record(
        state,
        Event::sync(
            "hd_screenshot.upload_refused",
            user_id,
            format!("Refused a 1080p screenshot copy: {why}"),
        )
        .detail(json!({
            "screenshotId": clip(screenshot_id, 64),
            "reason": why,
            "status": error.status.as_u16(),
        }))
        .warning(),
    )
    .await;

    error
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
    headers: HeaderMap,
    Path(screenshot_id): Path<String>,
    Json(payload): Json<UploadUrlRequest>,
) -> ApiResult<Json<Value>> {
    let user_id = user.0.id.as_str();

    if !valid_screenshot_id(&screenshot_id) {
        let error = ApiError::bad_request("invalid screenshot id");
        return Err(refused(
            &state,
            user_id,
            &screenshot_id,
            "the screenshot id is not valid",
            error,
        )
        .await);
    }

    let ext = payload.image_ext.trim_start_matches('.').to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        let error = ApiError::bad_request("unsupported image format");
        let why = format!("{:?} is not a supported image format", clip(&ext, 16));
        return Err(refused(&state, user_id, &screenshot_id, &why, error).await);
    }

    /* The declared size is what the quota is checked against and what the
    signed URL is bound to; `storage::upload` then holds it to the real bytes,
    so understating it buys nothing. */
    let length = payload.image_length.unwrap_or(0);
    let Some(limit) = storage::upload_limit(length) else {
        let error = ApiError::bad_request("imageLength is required");
        return Err(refused(
            &state,
            user_id,
            &screenshot_id,
            "the launcher did not say how large the image is",
            error,
        )
        .await);
    };

    if length > MAX_IMAGE_BYTES {
        let error = ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "image is too large");
        let why = format!("the image is {length} bytes, over the {MAX_IMAGE_BYTES} byte limit");
        return Err(refused(&state, user_id, &screenshot_id, &why, error).await);
    }

    if let Err(error) = storage::check_quota(&state, user_id, length).await {
        let why = if error.status == StatusCode::PAYLOAD_TOO_LARGE {
            "the account's storage quota is full"
        } else {
            "the account's storage quota could not be checked"
        };
        return Err(refused(&state, user_id, &screenshot_id, why, error).await);
    }

    /* Flat, uuid-named keys: which screenshot a file belongs to lives in the
    database, so nothing from the request ends up in a filesystem path. */
    let key = storage_key(user_id, &format!("{}.{ext}", Uuid::new_v4()));

    /* The launcher is about to connect to this address, and a server that is
    not reachable at it never sees that attempt. So the slot is recorded as
    it is handed out, with where the image is to be sent, and said plainly
    when that address cannot work. */
    let problem =
        crate::config::public_url_problem(&state.config.public_url, request_host(&headers));
    let origin = storage_origin(&state);

    let mut started = Event::sync(
        "hd_screenshot.upload_started",
        user_id,
        if problem.is_some() {
            "Upload slot given out, but its address can't be reached"
        } else {
            "Upload slot given out for a 1080p copy"
        },
    )
    .description(match &problem {
        Some(problem) => format!("{problem}\n\n{PUBLIC_URL_WHY}"),
        None => format!(
            "The launcher was told to send the image to {origin}/storage/\u{2026}. If no \
             \"uploaded\" event follows, the image did not arrive."
        ),
    })
    .detail(json!({
        "screenshotId": screenshot_id,
        "declaredBytes": length,
        "storageOrigin": origin,
        "requestHost": request_host(&headers).map(|host| clip(host, 200)),
    }))
    .size(length);
    if problem.is_some() {
        started = started.warning();
    }
    events::record(&state, started).await;

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
            let origin = storage_origin(&state);
            let reason = format!(
                "the image never reached storage. The launcher was told to send it to {origin} \
                 (HYDRA_SERVER_PUBLIC_URL): check that the machine running the launcher can \
                 connect to that address"
            );

            events::record(
                &state,
                Event::sync(
                    "hd_screenshot.upload_failed",
                    &user.0.id,
                    "1080p copy not saved: the image never reached storage",
                )
                .description(format!(
                    "The launcher asked to save the copy, but the image is not in storage, so \
                     the upload to the storage address did not complete. It was told to send it \
                     to {origin}.\n\n{PUBLIC_URL_WHY}"
                ))
                .detail(json!({
                    "screenshotId": screenshot_id,
                    "stage": "upload",
                    "storageOrigin": origin,
                    "reportedBy": "server",
                }))
                .warning(),
            )
            .await;

            /* `message` stays the code the launcher matches on; the sentence
            for a person goes beside it. */
            return Err(ApiError::new(StatusCode::CONFLICT, UPLOAD_INCOMPLETE_CODE)
                .with_extra(json!({ "detail": reason })));
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

    let replaced = previous.is_some();
    if let Some(previous) = previous {
        if previous != key {
            storage::delete_object(&state, &previous).await;
        }
    }

    events::record(
        &state,
        Event::sync(
            "hd_screenshot.uploaded",
            &user.0.id,
            format!(
                "Saved a 1080p screenshot copy ({}\u{d7}{}, {visibility})",
                payload.width, payload.height
            ),
        )
        .detail(json!({
            "screenshotId": screenshot_id,
            "width": payload.width,
            "height": payload.height,
            "visibility": visibility,
            "replacedPreviousCopy": replaced,
        }))
        .size(size_in_bytes),
    )
    .await;

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

        events::record(
            &state,
            Event::sync(
                "hd_screenshot.deleted",
                &user.0.id,
                "Deleted a 1080p screenshot copy",
            )
            .detail(json!({ "screenshotId": screenshot_id })),
        )
        .await;
    }

    Ok(Json(json!({ "ok": true })))
}

/// The steps of a copy's upload, in the words the history uses for them. What
/// the launcher reports names one of these.
const FAILURE_STAGES: &[(&str, &str, &str)] = &[
    ("read", "reading the file", "reading the screenshot file"),
    ("compress", "compressing", "compressing the screenshot"),
    (
        "upload-url",
        "asking for a slot",
        "asking this server for an upload slot",
    ),
    (
        "upload",
        "sending the image",
        "sending the image to the storage address",
    ),
    ("save", "saving", "attaching the image to the screenshot"),
];

/// The most failure reports one account's history takes in an hour. A launcher
/// retries on a growing delay, so a healthy one never gets near this; it
/// stops a client in a loop from filling the history.
const FAILURE_REPORTS_PER_HOUR: i64 = 120;

/// What the launcher knows about why a copy did not upload.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureReport {
    pub stage: String,
    #[serde(default)]
    pub message: Option<String>,
    /// A network error code such as `ECONNREFUSED`.
    #[serde(default)]
    pub code: Option<String>,
    /// The HTTP status of an answer, when there was one.
    #[serde(default)]
    pub status: Option<i64>,
    /// Where the launcher was sending to: the address, not the token.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub attempts: Option<i64>,
    #[serde(default)]
    pub will_retry: Option<bool>,
}

/// POST /profile/hd-screenshots/{screenshotId}/failure
///
/// The server can only see an upload that reaches it. When the launcher cannot
/// connect to the address it was given, or fails before it asks for one, there
/// is nothing here to log, and the history would show nothing wrong. The
/// launcher says so instead, and it is written down next to the rest.
pub async fn report_failure(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(screenshot_id): Path<String>,
    Json(report): Json<FailureReport>,
) -> ApiResult<Json<Value>> {
    if !valid_screenshot_id(&screenshot_id) {
        return Err(ApiError::bad_request("invalid screenshot id"));
    }

    let Some((_, short, doing)) = FAILURE_STAGES
        .iter()
        .find(|(stage, _, _)| *stage == report.stage)
    else {
        return Err(ApiError::bad_request("unknown stage"));
    };

    let since = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events
         WHERE user_id = ? AND kind = 'hd_screenshot.upload_failed' AND at >= ?",
    )
    .bind(&user.0.id)
    .bind(&since)
    .fetch_one(&state.pool)
    .await?;
    if recent >= FAILURE_REPORTS_PER_HOUR {
        return Ok(Json(json!({ "ok": true, "recorded": false })));
    }

    let message = report.message.as_deref().map(|text| clip(text, 300));
    let code = report.code.as_deref().map(|text| clip(text, 64));
    let target = report.target.as_deref().map(|text| clip(text, 200));

    let what = [
        message.clone().filter(|text| !text.is_empty()),
        code.clone().filter(|text| !text.is_empty()),
        report
            .status
            .map(|status| format!("the server answered {status}")),
    ]
    .into_iter()
    .flatten()
    .next()
    .unwrap_or_else(|| "no reason was given".to_string());

    let outlook = match report.will_retry {
        Some(true) => " It will try again.",
        Some(false) => " It has given up on this one.",
        None => "",
    };

    /* An upload that could not connect is almost always this server's own
    address not being one the launcher can use, and the server can tell. */
    let hint = (report.stage == "upload" && report.status.is_none())
        .then(|| {
            crate::config::public_url_problem(&state.config.public_url, request_host(&headers))
        })
        .flatten()
        .map(|problem| format!("\n\n{problem}\n\n{PUBLIC_URL_WHY}"))
        .unwrap_or_default();

    events::record(
        &state,
        Event::sync(
            "hd_screenshot.upload_failed",
            &user.0.id,
            format!("1080p copy failed ({short}): {}", shorten(&what, 60)),
        )
        .description(format!(
            "The launcher could not upload a 1080p screenshot copy while {doing}.\n\n\
             What it reported: {what}.{outlook}{hint}"
        ))
        .detail(json!({
            "screenshotId": screenshot_id,
            "stage": report.stage,
            "message": message,
            "code": code,
            "status": report.status,
            "target": target,
            "attempts": report.attempts,
            "willRetry": report.will_retry,
            "storageOrigin": storage_origin(&state),
            "reportedBy": "launcher",
        }))
        .warning(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "recorded": true })))
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
            HeaderMap::new(),
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
                HeaderMap::new(),
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
            HeaderMap::new(),
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

    /* What the history says, oldest first: kind, severity and summary. */
    async fn history(server: &TestServer) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT kind, severity, summary FROM events
             WHERE kind LIKE 'hd_screenshot.%' ORDER BY id ASC",
        )
        .fetch_all(&server.state.pool)
        .await
        .expect("the history")
    }

    /* The longer text behind an event's title, newest event of the kind. */
    async fn description_of(server: &TestServer, kind: &str) -> String {
        detail_of(server, kind).await["description"]
            .as_str()
            .expect("a description")
            .to_string()
    }

    async fn detail_of(server: &TestServer, kind: &str) -> Value {
        let detail: String =
            sqlx::query_scalar("SELECT detail FROM events WHERE kind = ? ORDER BY id DESC LIMIT 1")
                .bind(kind)
                .fetch_one(&server.state.pool)
                .await
                .expect("an event with detail");

        serde_json::from_str(&detail).expect("json detail")
    }

    async fn report(
        server: &TestServer,
        host: Option<&str>,
        id: &str,
        report: FailureReport,
    ) -> ApiResult<Value> {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(HOST, host.parse().expect("a host"));
        }

        report_failure(
            State(server.state.clone()),
            server.user("alice"),
            headers,
            Path(id.to_string()),
            Json(report),
        )
        .await
        .map(|Json(body)| body)
    }

    fn failure(stage: &str, message: &str) -> FailureReport {
        FailureReport {
            stage: stage.to_string(),
            message: Some(message.to_string()),
            code: None,
            status: None,
            target: None,
            attempts: None,
            will_retry: None,
        }
    }

    #[tokio::test]
    async fn a_finished_upload_is_written_to_the_history_with_its_size() {
        let server = TestServer::start().await;

        let url = reserve(&server, "alice", "shot-1", 5).await;
        land(&server, &url, b"12345").await;
        attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("saved");

        let kinds: Vec<_> = history(&server)
            .await
            .into_iter()
            .map(|(kind, severity, _)| (kind, severity))
            .collect();
        assert_eq!(
            kinds,
            [
                (
                    "hd_screenshot.upload_started".to_string(),
                    "info".to_string()
                ),
                ("hd_screenshot.uploaded".to_string(), "info".to_string()),
            ]
        );

        let size: i64 = server
            .scalar("SELECT size_bytes FROM events WHERE kind = 'hd_screenshot.uploaded'")
            .await;
        assert_eq!(size, 5, "the bytes that are on disk");

        let user: String = server
            .scalar("SELECT user_id FROM events WHERE kind = 'hd_screenshot.uploaded'")
            .await;
        assert_eq!(user, "alice");

        let detail = detail_of(&server, "hd_screenshot.uploaded").await;
        assert_eq!(detail["screenshotId"], "shot-1");
        assert_eq!(detail["width"], 1920);
        assert_eq!(detail["replacedPreviousCopy"], false);
    }

    #[tokio::test]
    async fn a_copy_whose_bytes_never_arrived_is_written_up_and_says_where_they_were_to_go() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 5).await;

        let refusal = attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect_err("nothing was uploaded");

        assert_eq!(refusal.status, StatusCode::CONFLICT);
        assert_eq!(
            refusal.message, UPLOAD_INCOMPLETE_CODE,
            "the code the launcher matches on is unchanged"
        );
        let sentence = refusal.extra.as_ref().expect("a sentence")["detail"]
            .as_str()
            .expect("text");
        assert!(sentence.contains("never reached storage"), "{sentence}");
        assert!(sentence.contains("HYDRA_SERVER_PUBLIC_URL"), "{sentence}");

        let failed: Vec<_> = history(&server)
            .await
            .into_iter()
            .filter(|(kind, _, _)| kind == "hd_screenshot.upload_failed")
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].1, "warning");
        assert!(
            failed[0].2.contains("never reached storage"),
            "{}",
            failed[0].2
        );
        assert!(failed[0].2.chars().count() < 80, "a title: {}", failed[0].2);

        let description = description_of(&server, "hd_screenshot.upload_failed").await;
        assert!(
            description.contains(&server.state.config.public_url),
            "names the address it was to be sent to: {description}"
        );
        assert!(
            description.contains("localhost means the launcher itself"),
            "says why the address matters: {description}"
        );

        assert!(
            find(&server, "alice", "alice", "shot-1").await.is_none(),
            "and nothing was recorded as a copy"
        );
    }

    #[tokio::test]
    async fn a_refused_upload_is_written_up_with_the_reason() {
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

        for (ext, length) in [("exe", Some(10)), ("jpg", None), ("jpg", Some(2_000_000))] {
            let _ = upload_url(
                State(server.state.clone()),
                server.user("alice"),
                HeaderMap::new(),
                Path("shot-1".to_string()),
                Json(UploadUrlRequest {
                    image_ext: ext.to_string(),
                    image_length: length,
                }),
            )
            .await
            .expect_err("a refusal");
        }

        let refused = history(&server).await;
        assert_eq!(refused.len(), 3);
        assert!(refused
            .iter()
            .all(|(kind, severity, _)| kind == "hd_screenshot.upload_refused"
                && severity == "warning"));
        assert!(
            refused[0].2.contains("not a supported image format"),
            "{}",
            refused[0].2
        );
        assert!(
            refused[1].2.contains("did not say how large"),
            "{}",
            refused[1].2
        );
        assert!(refused[2].2.contains("quota is full"), "{}", refused[2].2);
    }

    #[tokio::test]
    async fn a_slot_handed_out_at_an_address_nobody_can_reach_is_called_out() {
        let mut server = TestServer::start().await;

        /* What a server started without HYDRA_SERVER_PUBLIC_URL hands out. */
        let mut config = (*server.state.config).clone();
        config.public_url = "http://0.0.0.0:8788".to_string();
        server.state.config = std::sync::Arc::new(config);

        let _ = reserve(&server, "alice", "shot-1", 5).await;

        let started = history(&server).await;
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "hd_screenshot.upload_started");
        assert_eq!(started[0].1, "warning");
        assert!(
            started[0].2.contains("can't be reached"),
            "{}",
            started[0].2
        );
        assert!(
            started[0].2.chars().count() < 80,
            "a title: {}",
            started[0].2
        );

        let description = description_of(&server, "hd_screenshot.upload_started").await;
        assert!(description.contains("0.0.0.0"), "{description}");
        assert!(
            description.contains("HYDRA_SERVER_PUBLIC_URL"),
            "{description}"
        );
        assert!(
            description.contains("localhost means the launcher itself"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn deleting_a_copy_is_written_up_once() {
        let server = TestServer::start().await;
        let url = reserve(&server, "alice", "shot-1", 5).await;
        land(&server, &url, b"12345").await;
        attach(&server, "alice", "shot-1", &url, "PUBLIC")
            .await
            .expect("saved");

        for _ in 0..2 {
            let _ = delete(
                State(server.state.clone()),
                server.user("alice"),
                Path("shot-1".to_string()),
            )
            .await
            .expect("deleted");
        }

        let deleted = history(&server)
            .await
            .into_iter()
            .filter(|(kind, _, _)| kind == "hd_screenshot.deleted")
            .count();
        assert_eq!(deleted, 1, "the repeat had nothing to delete");
    }

    #[tokio::test]
    async fn the_launcher_can_say_why_an_upload_failed_and_it_lands_in_the_history() {
        let server = TestServer::start().await;

        let mut upload = failure("upload", "connect ECONNREFUSED 10.0.0.9:8788");
        upload.code = Some("ECONNREFUSED".to_string());
        upload.target = Some("http://10.0.0.9:8788".to_string());
        upload.attempts = Some(2);
        upload.will_retry = Some(true);

        let answer = report(&server, None, "shot-1", upload)
            .await
            .expect("recorded");
        assert_eq!(answer["recorded"], true);

        let entry = history(&server).await;
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].0, "hd_screenshot.upload_failed");
        assert_eq!(entry[0].1, "warning");
        /* The title is what the list shows: which step, and the error. */
        assert_eq!(
            entry[0].2,
            "1080p copy failed (sending the image): connect ECONNREFUSED 10.0.0.9:8788"
        );

        let description = description_of(&server, "hd_screenshot.upload_failed").await;
        assert!(
            description.contains("sending the image to the storage address"),
            "{description}"
        );
        assert!(
            description.contains("What it reported: connect ECONNREFUSED 10.0.0.9:8788."),
            "{description}"
        );
        assert!(description.contains("It will try again."), "{description}");
        assert!(
            !description.contains("HYDRA_SERVER_PUBLIC_URL"),
            "this server's address is not what is wrong here: {description}"
        );

        let detail = detail_of(&server, "hd_screenshot.upload_failed").await;
        assert_eq!(detail["stage"], "upload");
        assert_eq!(detail["code"], "ECONNREFUSED");
        assert_eq!(detail["target"], "http://10.0.0.9:8788");
        assert_eq!(detail["attempts"], 2);
        assert_eq!(detail["reportedBy"], "launcher");
    }

    #[tokio::test]
    async fn a_failure_to_connect_names_this_servers_own_address_when_that_is_the_problem() {
        let mut server = TestServer::start().await;
        let mut config = (*server.state.config).clone();
        config.public_url = "http://0.0.0.0:8788".to_string();
        server.state.config = std::sync::Arc::new(config);

        report(
            &server,
            Some("192.168.1.10:8788"),
            "shot-1",
            failure("upload", "connect EADDRNOTAVAIL"),
        )
        .await
        .expect("recorded");

        let description = description_of(&server, "hd_screenshot.upload_failed").await;
        assert!(description.contains("0.0.0.0"), "{description}");
        assert!(
            description.contains("HYDRA_SERVER_PUBLIC_URL"),
            "{description}"
        );
        assert!(
            description.contains("localhost means the launcher itself"),
            "{description}"
        );

        /* An answer from the server means it was reachable, so its address is
        not what to blame. */
        let mut refused = failure("upload", "Request failed with status code 403");
        refused.status = Some(403);
        report(&server, Some("192.168.1.10:8788"), "shot-2", refused)
            .await
            .expect("recorded");
        let description = description_of(&server, "hd_screenshot.upload_failed").await;
        assert!(
            !description.contains("HYDRA_SERVER_PUBLIC_URL"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn a_failure_report_is_checked_and_bounded() {
        let server = TestServer::start().await;

        let unknown = report(&server, None, "shot-1", failure("teleport", "x"))
            .await
            .expect_err("not a step of an upload");
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);

        let bad_id = report(&server, None, "../x", failure("upload", "x"))
            .await
            .expect_err("not an official id");
        assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);

        /* Control characters and length are the sender's to choose, so neither
        gets into the history. */
        let long = format!("bad\n{}\u{0007}", "x".repeat(1_000));
        report(&server, None, "shot-1", failure("compress", &long))
            .await
            .expect("recorded");
        let message = detail_of(&server, "hd_screenshot.upload_failed").await["message"]
            .as_str()
            .expect("text")
            .to_string();
        assert_eq!(message.chars().count(), 300);
        assert!(!message.chars().any(char::is_control));

        /* A long error is cut for the title and whole in the description. */
        let title: String = sqlx::query_scalar(
            "SELECT summary FROM events WHERE kind = 'hd_screenshot.upload_failed'
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&server.state.pool)
        .await
        .expect("a title");
        assert!(title.chars().count() < 100, "{title}");
        assert!(title.ends_with('\u{2026}'), "{title}");
        assert!(description_of(&server, "hd_screenshot.upload_failed")
            .await
            .contains(&"x".repeat(290)));

        /* A report with nothing in it still says something. */
        let mut empty = failure("save", "");
        empty.message = None;
        report(&server, None, "shot-1", empty)
            .await
            .expect("recorded");
        assert!(description_of(&server, "hd_screenshot.upload_failed")
            .await
            .contains("no reason was given"));

        /* A client in a loop fills the history only so far. */
        for _ in 0..FAILURE_REPORTS_PER_HOUR {
            report(&server, None, "shot-1", failure("read", "again"))
                .await
                .expect("answered");
        }
        let kept: i64 = server
            .scalar("SELECT COUNT(*) FROM events WHERE kind = 'hd_screenshot.upload_failed'")
            .await;
        assert_eq!(kept, FAILURE_REPORTS_PER_HOUR);

        let over = report(&server, None, "shot-1", failure("read", "once more"))
            .await
            .expect("still answered, so the launcher is not left retrying the report");
        assert_eq!(over["recorded"], false);
    }
}
