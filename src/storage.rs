use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use chrono::Utc;
use futures::StreamExt;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

const UPLOAD_TOKEN_TTL_SECONDS: i64 = 60 * 60;
const DOWNLOAD_TOKEN_TTL_SECONDS: i64 = 60 * 15;

/// Storage URLs work like S3 presigned URLs: the launcher PUTs/GETs raw
/// bytes with no auth header, so all authorization lives in a short-lived
/// signed token embedded in the URL itself.
#[derive(Serialize, Deserialize)]
pub struct StorageClaims {
    /// "put" | "get"
    pub op: String,
    /// storage key relative to the storage dir, e.g. "artifacts/<id>.tar"
    pub key: String,
    /// Size the uploader declared, in bytes (`put` only). Always enforced by
    /// `upload`, so zero means an empty object rather than "no limit".
    pub max: u64,
    pub exp: i64,
    /// Expected lowercase-hex SHA-256 of the uploaded bytes (Cloud Save V2
    /// blobs). When set the body is hashed as it streams and rejected on
    /// mismatch, so a content-addressed key can never end up holding bytes
    /// that don't match the hash it is named after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Every table the per-user quota is measured against, as
/// (metric kind, table, size column).
///
/// This is the one definition: the quota check, the admin panel's per-user
/// column, the panel's stored-bytes total and the `/metrics` gauge are all
/// generated from this list, so a newly metered table cannot reach one of
/// them and miss another.
pub const METERED_TABLES: &[(&str, &str, &str)] = &[
    ("cloud_saves", "cloud_save_blobs", "size_in_bytes"),
    ("backups", "artifacts", "artifact_length_in_bytes"),
    (
        "emulation_saves",
        "emulation_saves",
        "artifact_length_in_bytes",
    ),
    ("artwork", "game_artwork", "size_in_bytes"),
    ("souvenirs", "souvenirs", "size_in_bytes"),
];

/// SQL summing every metered table for one owner: save backups, emulation
/// saves, uploaded custom images, achievement souvenirs and Cloud Save V2
/// blobs.
///
/// `owner` is the SQL expression naming that owner — `?1` for a bound
/// parameter, `u.id` to correlate with a joined `users` row. Callers pass a
/// literal, never anything from a request.
///
/// Profile banners and avatars are deliberately absent: each is capped at
/// `images::MAX_IMAGE_BYTES` and replaces the file it supersedes, so a user
/// holds at most one of each and the total can't grow.
pub fn used_bytes_expr(owner: &str) -> String {
    sum_of_metered(&format!(" WHERE t.user_id = {owner}"))
}

/// SQL summing every metered table across all users.
pub fn stored_bytes_expr() -> String {
    sum_of_metered("")
}

fn sum_of_metered(predicate: &str) -> String {
    METERED_TABLES
        .iter()
        .map(|(_, table, column)| {
            format!("(SELECT COALESCE(SUM(t.{column}), 0) FROM {table} t{predicate})")
        })
        .collect::<Vec<_>>()
        .join("\n      + ")
}

/// Total bytes a user is storing here.
///
/// V2 blobs are counted once per distinct hash, which is also how they are
/// stored — a file duplicated across variants or games costs nothing extra.
pub async fn used_bytes(state: &AppState, user_id: &str) -> ApiResult<i64> {
    let used: i64 = sqlx::query_scalar(&format!("SELECT {}", used_bytes_expr("?1")))
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;

    Ok(used)
}

// ---------------------------------------------------------------------------
// The quota
// ---------------------------------------------------------------------------

/// What a full account is told, wherever it is told. The launcher shows this
/// verbatim, so it is written for the person at the keyboard.
const QUOTA_MESSAGE: &str = "storage quota exceeded — free up space or ask the server admin";

/// The refusal a full account gets, from a presign and from the upload itself.
pub fn quota_error() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, QUOTA_MESSAGE)
}

/// The one quota boundary: `used` bytes plus `incoming` more, against a
/// `quota` where zero means unlimited.
///
/// The admin panel counts an account as being at its quota with `used >= quota`.
/// For endpoints that require a positive declared size (those using
/// [`upload_limit`]), this matches the refusal boundary for an additional byte,
/// so the panel's count corresponds to users whose next non-empty upload will be
/// refused.
pub fn exceeds_quota(quota: u64, used: i64, incoming: i64) -> bool {
    quota > 0 && used.saturating_add(incoming) > signed(quota)
}

/// Byte counts are summed as `i64` in SQL, and a `u64` past that is not a real
/// upload.
fn signed(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// Refuses an upload the owner has no room for, before anything is written
/// down.
///
/// This is a prediction: at presign time the file doesn't exist here yet, so
/// it runs against the size the launcher declares. That is worth doing anyway
/// — it gives the launcher an answer it can act on before it spends the
/// upload, and running it before the row is inserted keeps a refusal from
/// leaving one behind. [`upload`] is what actually holds the line, against the
/// bytes that really arrive.
pub async fn check_quota(state: &AppState, user_id: &str, incoming: i64) -> ApiResult<()> {
    let quota = state.settings.read().await.max_bytes_per_user;
    if quota == 0 {
        return Ok(());
    }

    if exceeds_quota(quota, used_bytes(state, user_id).await?, incoming) {
        return Err(quota_error());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Enforcing the quota on the bytes themselves
// ---------------------------------------------------------------------------

/// How often the owner's stored total is re-read while a body streams.
///
/// Between reads [`InFlightUploads`] covers what the owner's other uploads are
/// writing; the re-read is what picks up one that *finished* while this one
/// was streaming, and any presign made since it started.
const QUOTA_REFRESH_BYTES: u64 = 8 * 1024 * 1024;

/// Where a storage key's bytes land in the quota.
pub struct QuotaTarget {
    /// The account they are charged to.
    user_id: String,
    /// What the database already counts for this key: the size a presign
    /// reserved on the row, or the size of the object this upload replaces.
    /// It is subtracted before the incoming bytes are added, so a reservation
    /// is never charged twice and a re-upload only pays the difference.
    counted_bytes: i64,
}

impl QuotaTarget {
    fn new(user_id: &str, counted_bytes: Option<i64>) -> Self {
        Self {
            user_id: user_id.to_string(),
            counted_bytes: counted_bytes.unwrap_or(0).max(0),
        }
    }
}

/// Resolves a storage key to the row its bytes will be counted in.
///
/// The shapes mirror [`finalize_upload`]: an upload lands in the row this
/// finds, so the two have to agree about which row that is. `None` means
/// either that nothing meters the key — profile images, which are capped
/// individually and replace themselves — or that the row it belonged to is
/// gone, leaving no account to charge.
async fn quota_target(state: &AppState, key: &str) -> ApiResult<Option<QuotaTarget>> {
    if let Some(id) = key
        .strip_prefix("artifacts/")
        .and_then(|rest| rest.strip_suffix(".tar"))
    {
        return row_target(
            state,
            "SELECT user_id, artifact_length_in_bytes FROM artifacts WHERE id = ?",
            id,
        )
        .await;
    }

    if let Some(id) = key
        .strip_prefix("emulation-saves/")
        .and_then(|rest| rest.strip_suffix(".bin"))
    {
        return row_target(
            state,
            "SELECT user_id, artifact_length_in_bytes FROM emulation_saves WHERE id = ?",
            id,
        )
        .await;
    }

    /* The rest are stored under their owner's own prefix, so the account is
       known even before a row exists to carry the size. */
    if let Some((user_id, _)) = owner_prefixed(key, "images/souvenirs/") {
        let counted = sqlx::query_scalar("SELECT size_in_bytes FROM souvenirs WHERE image_key = ?")
            .bind(key)
            .fetch_optional(&state.pool)
            .await?;

        return Ok(Some(QuotaTarget::new(user_id, counted)));
    }

    if let Some((user_id, _)) = owner_prefixed(key, "images/artwork/") {
        let counted =
            sqlx::query_scalar("SELECT size_in_bytes FROM game_artwork WHERE storage_key = ?")
                .bind(key)
                .fetch_optional(&state.pool)
                .await?;

        return Ok(Some(QuotaTarget::new(user_id, counted)));
    }

    if let Some((user_id, hash)) = owner_prefixed(key, "cloud-saves/") {
        let counted = sqlx::query_scalar(
            "SELECT size_in_bytes FROM cloud_save_blobs WHERE user_id = ? AND hash = ?",
        )
        .bind(user_id)
        .bind(hash)
        .fetch_optional(&state.pool)
        .await?;

        return Ok(Some(QuotaTarget::new(user_id, counted)));
    }

    Ok(None)
}

async fn row_target(state: &AppState, sql: &str, id: &str) -> ApiResult<Option<QuotaTarget>> {
    let row: Option<(String, i64)> = sqlx::query_as(sql)
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;

    Ok(row.map(|(user_id, counted_bytes)| QuotaTarget {
        user_id,
        counted_bytes: counted_bytes.max(0),
    }))
}

/// A `{prefix}{user_id}/{rest}` key split into its owner and the rest.
fn owner_prefixed<'a>(key: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let (user_id, rest) = key.strip_prefix(prefix)?.split_once('/')?;
    (!user_id.is_empty() && !rest.is_empty()).then_some((user_id, rest))
}

/// Bytes a key may still hold: the whole quota, less everything else its owner
/// is storing.
async fn remaining_quota(state: &AppState, quota: u64, target: &QuotaTarget) -> ApiResult<i64> {
    let used = used_bytes(state, &target.user_id).await?;

    Ok((signed(quota) - (used - target.counted_bytes)).max(0))
}

/// What the uploads currently streaming have written beyond what the database
/// counts for them, keyed by storage key.
///
/// The quota is measured from the database, which only learns an upload's real
/// size once it lands. Two bodies streaming at once would otherwise each be
/// measured against a total that ignores the other, and both fit in the same
/// free space — the launcher uploads Cloud Save V2 blobs in parallel, so that
/// is ordinary traffic and not only abuse. Every upload in progress publishes
/// itself here for the length of its request, so each check sees what the
/// others are already holding.
#[derive(Default)]
pub struct InFlightUploads {
    /// Deliberately not an async lock: nothing awaits while it is held, and
    /// the entry has to be released from `Drop`.
    entries: Mutex<HashMap<String, InFlight>>,
}

struct InFlight {
    user_id: String,
    /// Bytes written past what the database counts for the key — what this
    /// upload is about to add to its owner's total.
    extra_bytes: u64,
}

impl InFlightUploads {
    /// A poisoned lock is nothing to abort over: the map holds plain numbers,
    /// and every entry is released with its request.
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, InFlight>> {
        self.entries.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn publish(&self, key: &str, user_id: &str, extra_bytes: u64) {
        self.entries()
            .entry(key.to_string())
            .and_modify(|entry| entry.extra_bytes = extra_bytes)
            .or_insert_with(|| InFlight {
                user_id: user_id.to_string(),
                extra_bytes,
            });
    }

    fn release(&self, key: &str) {
        self.entries().remove(key);
    }

    /// Bytes the owner's *other* in-flight uploads are about to add.
    fn extra_bytes(&self, user_id: &str, except_key: &str) -> i64 {
        self.entries()
            .iter()
            .filter(|(key, entry)| entry.user_id == user_id && key.as_str() != except_key)
            .fold(0i64, |total, (_, entry)| {
                total.saturating_add(signed(entry.extra_bytes))
            })
    }
}

/// The quota, enforced against one upload's body as it arrives.
///
/// Holds the free space its owner had when the request started, refreshes it
/// as the body streams, and keeps this upload visible to the checks the
/// owner's other uploads are making at the same time.
struct QuotaGate {
    quota: u64,
    key: String,
    target: QuotaTarget,
    uploads: Arc<InFlightUploads>,
    /// Bytes this key may hold, as of the last database read.
    budget: i64,
    /// Bytes written at that read.
    read_at: u64,
    /// The size the uploader said this object would be. Counted from the very
    /// first check, so a declared size that cannot fit is refused before a
    /// byte is written and the space is held while it is being filled.
    declared: u64,
}

impl QuotaGate {
    /// `None` when there is nothing to enforce: no quota configured, or a key
    /// no account is charged for.
    async fn open(state: &AppState, key: &str, declared: u64) -> ApiResult<Option<Self>> {
        let quota = state.settings.read().await.max_bytes_per_user;
        if quota == 0 {
            return Ok(None);
        }

        let Some(target) = quota_target(state, key).await? else {
            return Ok(None);
        };

        Ok(Some(Self {
            budget: remaining_quota(state, quota, &target).await?,
            quota,
            key: key.to_string(),
            target,
            uploads: state.uploads.clone(),
            read_at: 0,
            declared,
        }))
    }

    /// Refuses the upload once `written` bytes would take its owner past the
    /// quota. Called for every chunk, so a body that understated its size is
    /// stopped while it streams instead of being noticed after it has landed.
    async fn check(&mut self, state: &AppState, written: u64) -> ApiResult<()> {
        if written >= self.read_at.saturating_add(QUOTA_REFRESH_BYTES) {
            self.budget = remaining_quota(state, self.quota, &self.target).await?;
            self.read_at = written;
        }

        let footprint = written.max(self.declared);
        self.uploads.publish(
            &self.key,
            &self.target.user_id,
            footprint.saturating_sub(self.target.counted_bytes as u64),
        );

        let others = self.uploads.extra_bytes(&self.target.user_id, &self.key);
        if signed(footprint) > self.budget - others {
            crate::events::record(
                state,
                crate::events::Event::sync(
                    "storage.quota_exceeded",
                    &self.target.user_id,
                    "Upload refused — quota full",
                )
                .detail(serde_json::json!({
                    "key": &self.key,
                    "writtenBytes": written,
                    "declaredBytes": self.declared,
                    "quotaBytes": self.quota,
                }))
                .warning(),
            )
            .await;

            return Err(quota_error());
        }

        Ok(())
    }
}

impl Drop for QuotaGate {
    fn drop(&mut self) {
        self.uploads.release(&self.key);
    }
}

/// Storage key for a Cloud Save V2 blob. Content-addressed and namespaced per
/// user so one user's bytes are never served to another.
pub fn cloud_save_blob_key(user_id: &str, hash: &str) -> String {
    format!("cloud-saves/{user_id}/{hash}")
}

/// Presigned PUT for a Cloud Save V2 blob, bound to the hash it must contain.
///
/// Takes a plain size rather than an `upload_limit`: a zero-byte save file is
/// legitimate content, and `size_limit` bounds it either way.
pub fn sign_blob_upload_url(
    state: &AppState,
    user_id: &str,
    hash: &str,
    size_bytes: u64,
) -> String {
    sign_url_with_hash(
        state,
        "put",
        &cloud_save_blob_key(user_id, hash),
        size_bytes,
        UPLOAD_TOKEN_TTL_SECONDS,
        Some(hash.to_string()),
    )
}

/// Byte budget for an upload token, from the size a caller declared.
///
/// `None` for a missing, zero or negative declaration. `sign_upload_url` takes
/// nothing else, so an endpoint that has no real size to state cannot mint a
/// token at all — a max of zero used to mean "no limit".
pub fn upload_limit(declared_bytes: i64) -> Option<NonZeroU64> {
    NonZeroU64::new(u64::try_from(declared_bytes).ok()?)
}

pub fn sign_upload_url(state: &AppState, key: &str, max_bytes: NonZeroU64) -> String {
    sign_url(state, "put", key, max_bytes.get(), UPLOAD_TOKEN_TTL_SECONDS)
}

pub fn sign_download_url(state: &AppState, key: &str) -> String {
    sign_url(state, "get", key, 0, DOWNLOAD_TOKEN_TTL_SECONDS)
}

fn sign_url(state: &AppState, op: &str, key: &str, max: u64, ttl: i64) -> String {
    sign_url_with_hash(state, op, key, max, ttl, None)
}

fn sign_url_with_hash(
    state: &AppState,
    op: &str,
    key: &str,
    max: u64,
    ttl: i64,
    sha256: Option<String>,
) -> String {
    let claims = StorageClaims {
        op: op.to_string(),
        key: key.to_string(),
        max,
        exp: Utc::now().timestamp() + ttl,
        sha256,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.secret.as_bytes()),
    )
    .expect("failed to sign storage token");

    format!("{}/storage/{}", state.config.public_url, token)
}

/// Bytes a `put` claim declaring `declared` is allowed to write.
///
/// Every declared size gets the same slack, which covers metadata drift
/// between the launcher's stat() and the upload itself. There is no
/// "unlimited" case: a zero declaration still buys only the slack.
fn size_limit(declared: u64) -> u64 {
    declared
        .saturating_add(declared / 10)
        .saturating_add(1024 * 1024)
}

fn decode_token(state: &AppState, token: &str, expected_op: &str) -> ApiResult<StorageClaims> {
    let claims = decode::<StorageClaims>(
        token,
        &DecodingKey::from_secret(state.config.secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| ApiError::unauthorized("invalid or expired storage token"))?
    .claims;

    if claims.op != expected_op {
        return Err(ApiError::forbidden("wrong storage operation"));
    }

    if !is_safe_key(&claims.key) {
        return Err(ApiError::bad_request("invalid storage key"));
    }

    Ok(claims)
}

fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && !key.contains("..")
        && !key.starts_with('/')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

pub fn storage_path(state: &AppState, key: &str) -> std::path::PathBuf {
    state.config.storage_dir().join(key)
}

pub async fn delete_object(state: &AppState, key: &str) {
    let path = storage_path(state, key);
    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("failed to delete {}: {err}", path.display());
        }
    }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    /// Byte offset this request's body starts at (chunked uploads).
    pub offset: Option<u64>,
    /// Total size of the object being uploaded (chunked uploads).
    pub total: Option<u64>,
}

/// PUT /storage/{token}[?offset=&total=] — streams the request body to disk.
///
/// Without query params the whole object is expected in a single request.
/// With `offset`/`total` the launcher uploads sequential chunks kept small
/// enough to pass proxies that cap request bodies (Cloudflare caps them at
/// 100 MB on free plans); the object is finalized once `total` bytes have
/// arrived.
///
/// Two limits apply to the body: the size the token was signed for, and the
/// owner's remaining quota. The second is the one a client cannot talk its way
/// around — the presigns check what a launcher *says* it is about to store,
/// this checks what it actually sends.
pub async fn upload(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> ApiResult<StatusCode> {
    let claims = decode_token(&state, &token, "put")?;

    let path = storage_path(&state, &claims.key);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let (offset, total) = match (query.offset, query.total) {
        (None, None) => (0, None),
        (Some(offset), Some(total)) if offset < total => (offset, Some(total)),
        _ => return Err(ApiError::bad_request("invalid chunk range")),
    };

    /* Hash-bound keys (Cloud Save V2 blobs) are verified as the body streams,
       which only works when the whole object arrives in one request. The
       launcher uploads these in a single PUT; refuse chunking rather than
       quietly storing unverified bytes under a content-addressed name. */
    if claims.sha256.is_some() && (offset != 0 || total.is_some()) {
        return Err(ApiError::bad_request(
            "chunked upload is not supported for content-addressed objects",
        ));
    }

    let size_limit = size_limit(claims.max);

    if total.is_some_and(|total| total > size_limit) {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload exceeds declared size",
        ));
    }

    let temp_path = path.with_extension("uploading");

    /* Opened before a byte is written, and checked at `offset` so a chunked
       upload is re-measured on every chunk: what already landed still counts
       when the next one asks for room. A chunked upload that runs out of room
       partway through can never be finished, so the part of it already on disk
       goes too. */
    let mut gate = QuotaGate::open(&state, &claims.key, total.unwrap_or(claims.max)).await?;
    if let Some(gate) = gate.as_mut() {
        if let Err(err) = gate.check(&state, offset).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(err);
        }
    }

    let mut file = if offset == 0 {
        tokio::fs::File::create(&temp_path).await?
    } else {
        let existing = tokio::fs::metadata(&temp_path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);

        if existing != offset {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "chunk out of order — restart the upload from the beginning",
            ));
        }

        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&temp_path)
            .await?
    };

    let mut written: u64 = offset;
    let mut stream = body.into_data_stream();
    let mut digest = claims.sha256.as_ref().map(|_| Sha256::new());

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| ApiError::bad_request("upload interrupted"))?;

        if let Some(digest) = digest.as_mut() {
            digest.update(&chunk);
        }

        written += chunk.len() as u64;

        if written > size_limit || total.is_some_and(|total| written > total) {
            drop(file);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds declared size",
            ));
        }

        /* A body that understated its size gets no further than the byte that
           fills the quota, and the partial file goes with it — a refused
           upload leaves nothing on disk to sweep up later. */
        if let Some(gate) = gate.as_mut() {
            if let Err(err) = gate.check(&state, written).await {
                drop(file);
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(err);
            }
        }

        file.write_all(&chunk)
            .await
            .map_err(ApiError::from)?;
    }

    file.flush().await?;
    drop(file);

    let complete = match total {
        Some(total) => written >= total,
        None => true,
    };

    if !complete {
        return Ok(StatusCode::OK);
    }

    if let (Some(digest), Some(expected)) = (digest, claims.sha256.as_deref()) {
        let actual = hex::encode(digest.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&temp_path).await;

            /* Content-addressed storage rejecting its own bytes is either a
               corrupted transfer or someone trying to poison a hash — either
               way an operator wants to know it happened. */
            crate::events::record(
                &state,
                crate::events::Event::system(
                    "storage.hash_mismatch",
                    "Upload rejected — bytes did not match the declared SHA-256",
                )
                .detail(serde_json::json!({ "key": claims.key, "expected": expected }))
                .warning(),
            )
            .await;

            return Err(ApiError::bad_request(
                "uploaded bytes do not match the declared SHA-256",
            ));
        }
    }

    tokio::fs::rename(&temp_path, &path).await?;

    finalize_upload(&state, &claims.key, written).await?;
    state.metrics.add_uploaded(written);

    tracing::info!("stored {} ({} bytes)", claims.key, written);
    Ok(StatusCode::OK)
}

/// Marks the owning row as uploaded once the bytes are on disk.
async fn finalize_upload(state: &AppState, key: &str, written: u64) -> ApiResult<()> {
    let now = Utc::now().to_rfc3339();

    /* A profile image becomes the user's current one and the file it
       supersedes is deleted, so avatars and banners hold one file each
       instead of piling up outside the quota. */
    for (prefix, column) in [
        ("images/banners/", "banner_key"),
        ("images/avatars/", "avatar_key"),
    ] {
        if let Some(rest) = key.strip_prefix(prefix) {
            if let Some((user_id, _file)) = rest.split_once('/') {
                replace_profile_image(state, column, user_id, key).await?;
            }
        }
    }

    if let Some(id) = key
        .strip_prefix("artifacts/")
        .and_then(|rest| rest.strip_suffix(".tar"))
    {
        sqlx::query(
            "UPDATE artifacts SET is_uploaded = 1, artifact_length_in_bytes = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(written as i64)
        .bind(&now)
        .bind(id)
        .execute(&state.pool)
        .await?;
    }

    /* Souvenir screenshots are reserved before their bytes exist, so the row
       only counts against the quota — and only becomes visible on a profile —
       once the upload has actually landed. */
    if key.starts_with("images/souvenirs/") {
        crate::souvenirs::mark_uploaded(state, key, written).await?;
    }

    if let Some(id) = key
        .strip_prefix("emulation-saves/")
        .and_then(|rest| rest.strip_suffix(".bin"))
    {
        sqlx::query(
            "UPDATE emulation_saves
             SET is_uploaded = 1, artifact_length_in_bytes = ?, last_uploaded_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(written as i64)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(&state.pool)
        .await?;
    }

    Ok(())
}

/// Records `key` as the user's current banner or avatar and deletes the file
/// it replaced. `column` is a literal from [`finalize_upload`].
async fn replace_profile_image(
    state: &AppState,
    column: &str,
    user_id: &str,
    key: &str,
) -> ApiResult<()> {
    let previous: Option<String> =
        sqlx::query_scalar(&format!("SELECT {column} FROM users WHERE id = ?"))
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();

    sqlx::query(&format!("UPDATE users SET {column} = ? WHERE id = ?"))
        .bind(key)
        .bind(user_id)
        .execute(&state.pool)
        .await?;

    if let Some(previous) = previous {
        if previous != key {
            delete_object(state, &previous).await;
        }
    }

    Ok(())
}

/// GET /storage/{token} — streams a stored file back.
pub async fn download(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let claims = decode_token(&state, &token, "get")?;
    let path = storage_path(&state, &claims.key);

    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::not_found("object not found"))?;
    let length = file.metadata().await?.len();

    let stream = tokio_util::io::ReaderStream::new(file);
    state.metrics.add_downloaded(length);

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal("failed to build response"))?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// The zero that used to mean "no limit".
    #[test]
    fn a_missing_or_non_positive_size_yields_no_upload_limit() {
        assert_eq!(upload_limit(0), None);
        assert_eq!(upload_limit(-1), None);
        assert_eq!(upload_limit(i64::MIN), None);
    }

    #[test]
    fn a_positive_size_yields_that_limit() {
        assert_eq!(upload_limit(1).map(NonZeroU64::get), Some(1));
        assert_eq!(
            upload_limit(30 * MIB as i64).map(NonZeroU64::get),
            Some(30 * MIB)
        );
    }

    /// The quota check, the admin panel and the metrics gauge all read this
    /// list, so a table reaching one of them reaches every one.
    #[test]
    fn both_totals_cover_every_metered_table() {
        let per_user = used_bytes_expr("?1");
        let global = stored_bytes_expr();

        for (_, table, column) in METERED_TABLES {
            assert!(per_user.contains(&format!("FROM {table} t WHERE t.user_id = ?1")));
            assert!(per_user.contains(&format!("SUM(t.{column})")));
            assert!(global.contains(&format!("FROM {table} t)")));
        }

        assert_eq!(per_user.matches("SELECT COALESCE").count(), METERED_TABLES.len());
        assert_eq!(global.matches("SELECT COALESCE").count(), METERED_TABLES.len());
    }

    /// Correlating against a joined `users u` needs the inner predicate to
    /// name its own table, or a column added to `users` could capture it.
    #[test]
    fn the_owner_predicate_is_qualified() {
        assert!(used_bytes_expr("u.id").contains("WHERE t.user_id = u.id"));
        assert!(!stored_bytes_expr().contains("WHERE"));
    }

    #[test]
    fn every_declared_size_is_bounded() {
        assert_eq!(size_limit(0), MIB);
        assert_eq!(size_limit(10 * MIB), 10 * MIB + MIB + MIB);
        assert_eq!(size_limit(u64::MAX), u64::MAX);
    }

    #[test]
    fn a_zero_quota_is_unlimited() {
        assert!(!exceeds_quota(0, i64::MAX, i64::MAX));
    }

    /// The endpoints refuse `used + declared > quota`; the admin panel counts
    /// an account as being at its quota with `used >= quota`. No declared size
    /// is ever below one byte, so those are the same line — the panel's number
    /// is exactly the set of users whose next upload is refused.
    #[test]
    fn the_panel_boundary_is_the_refusal_boundary() {
        let quota = 100u64;

        for used in 0..=2 * quota as i64 {
            assert_eq!(
                exceeds_quota(quota, used, 1),
                used >= quota as i64,
                "at used = {used}"
            );
        }

        assert!(!exceeds_quota(quota, 99, 1));
        assert!(exceeds_quota(quota, 99, 2));
    }

    /// Sizes are summed as `i64` in SQL, and the quota is a `u64` an operator
    /// types. A saturating conversion keeps an absurd setting from wrapping
    /// into a quota of nearly nothing.
    #[test]
    fn an_absurd_quota_does_not_wrap() {
        assert_eq!(signed(u64::MAX), i64::MAX);
        assert!(!exceeds_quota(u64::MAX, i64::MAX - 1, 1));
    }

    /// Keys carrying their owner have to be split the way the endpoints build
    /// them, and nothing else may pass for one.
    #[test]
    fn owner_prefixed_keys_yield_their_owner() {
        assert_eq!(
            owner_prefixed("images/souvenirs/u1/shot.png", "images/souvenirs/"),
            Some(("u1", "shot.png"))
        );
        assert_eq!(
            owner_prefixed("cloud-saves/u1/abc123", "cloud-saves/"),
            Some(("u1", "abc123"))
        );

        assert_eq!(owner_prefixed("cloud-saves/u1", "cloud-saves/"), None);
        assert_eq!(owner_prefixed("cloud-saves//abc", "cloud-saves/"), None);
        assert_eq!(owner_prefixed("cloud-saves/u1/", "cloud-saves/"), None);
        assert_eq!(owner_prefixed("images/avatars/u1/a.png", "cloud-saves/"), None);
    }

    /// One upload only sees what the *others* are holding, or it would refuse
    /// itself the space it is already accounted for.
    #[test]
    fn an_upload_is_measured_against_the_other_uploads_only() {
        let uploads = InFlightUploads::default();

        uploads.publish("artifacts/a.tar", "alice", 10);
        uploads.publish("artifacts/b.tar", "alice", 25);
        uploads.publish("artifacts/c.tar", "bob", 1000);

        assert_eq!(uploads.extra_bytes("alice", "artifacts/a.tar"), 25);
        assert_eq!(uploads.extra_bytes("alice", "artifacts/b.tar"), 10);
        assert_eq!(uploads.extra_bytes("alice", "artifacts/none"), 35);
        assert_eq!(uploads.extra_bytes("bob", "artifacts/c.tar"), 0);
        assert_eq!(uploads.extra_bytes("carol", ""), 0);

        /* A later chunk replaces the figure rather than adding to it: each
           entry is a running total, not a delta. */
        uploads.publish("artifacts/a.tar", "alice", 40);
        assert_eq!(uploads.extra_bytes("alice", "artifacts/b.tar"), 40);

        uploads.release("artifacts/a.tar");
        assert_eq!(uploads.extra_bytes("alice", "artifacts/b.tar"), 0);
    }

    /// Every metered table has to be reachable from a storage key, or an
    /// upload lands in a row the quota gate could not find.
    #[test]
    fn every_metered_table_is_reachable_from_a_key() {
        let source = include_str!("storage.rs");
        let targets = source
            .split("async fn quota_target")
            .nth(1)
            .expect("quota_target is defined");

        for (_, table, _) in METERED_TABLES {
            assert!(targets.contains(&format!("FROM {table} ")), "{table}");
        }
    }

    // -----------------------------------------------------------------------
    // The quota gate, through `upload` itself
    // -----------------------------------------------------------------------

    use axum::body::Bytes;
    use std::path::PathBuf;

    /// A server on a scratch database and storage directory.
    ///
    /// The upload path is worth exercising against the real thing: what it
    /// allows depends on rows other endpoints wrote, on the settings, and on
    /// what is already on disk. A stand-in for those would only be testing
    /// the stand-in.
    struct TestServer {
        state: AppState,
        dir: PathBuf,
    }

    impl TestServer {
        async fn start(quota: u64) -> Self {
            let dir = std::env::temp_dir().join(format!("hydra-upload-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("scratch directory");

            let mut config = crate::config::Config::for_test();
            config.data_dir = dir.clone();

            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(config.database_path())
                        .create_if_missing(true),
                )
                .await
                .expect("scratch database");

            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("migrations");

            let mut settings = crate::state::RuntimeSettings::from_config(&config);
            settings.max_bytes_per_user = quota;

            let now = Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO users (id, display_name, created_at, last_seen_at)
                 VALUES ('alice', 'alice', ?, ?)",
            )
            .bind(&now)
            .bind(&now)
            .execute(&pool)
            .await
            .expect("a user to charge");

            let state = AppState {
                pool,
                config: Arc::new(config),
                http: reqwest::Client::new(),
                token_cache: Default::default(),
                settings: Arc::new(tokio::sync::RwLock::new(settings)),
                started_at: Utc::now(),
                metrics: Default::default(),
                uploads: Default::default(),
                login_guard: Default::default(),
                running_tasks: Default::default(),
                presence: Default::default(),
            };

            Self { state, dir }
        }

        /// A reserved emulation save, exactly as `create_upload_url` leaves
        /// one: a row carrying the declared size, and a token bound to it.
        async fn reserve(&self, declared: i64) -> Save {
            let id = uuid::Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();

            sqlx::query(
                "INSERT INTO emulation_saves
                   (id, user_id, platform, emulator, save_identity,
                    artifact_length_in_bytes, created_at, updated_at)
                 VALUES (?, 'alice', 'ps2', 'pcsx2', 'slot', ?, ?, ?)",
            )
            .bind(&id)
            .bind(declared)
            .bind(&now)
            .bind(&now)
            .execute(&self.state.pool)
            .await
            .expect("a reserved save");

            let key = format!("emulation-saves/{id}.bin");
            let url = sign_upload_url(
                &self.state,
                &key,
                upload_limit(declared).expect("a positive declared size"),
            );

            Save {
                id,
                token: url.rsplit('/').next().expect("a signed token").to_string(),
                path: storage_path(&self.state, &key),
            }
        }

        /// PUTs `frames` to the token, one body frame each — the quota is
        /// re-checked per frame, so this is what a body arriving in pieces
        /// looks like from the inside.
        async fn put(
            &self,
            save: &Save,
            query: UploadQuery,
            frames: Vec<Vec<u8>>,
        ) -> ApiResult<StatusCode> {
            let body = Body::from_stream(futures::stream::iter(
                frames
                    .into_iter()
                    .map(|frame| Ok::<_, std::io::Error>(Bytes::from(frame))),
            ));

            upload(
                State(self.state.clone()),
                Path(save.token.clone()),
                Query(query),
                body,
            )
            .await
        }

        /// `(is_uploaded, artifact_length_in_bytes)` as the row stands now.
        async fn row(&self, id: &str) -> (i64, i64) {
            sqlx::query_as(
                "SELECT is_uploaded, artifact_length_in_bytes FROM emulation_saves WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&self.state.pool)
            .await
            .expect("the reserved row")
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    struct Save {
        id: String,
        token: String,
        path: PathBuf,
    }

    impl Save {
        /// Where the bytes accumulate before they are complete.
        fn partial(&self) -> PathBuf {
            self.path.with_extension("uploading")
        }
    }

    const ONE_SHOT: UploadQuery = UploadQuery {
        offset: None,
        total: None,
    };

    /// The point of the gate: a token bound to one byte cannot spend an
    /// account's whole quota, however much its holder sends.
    #[tokio::test]
    async fn an_upload_that_outgrows_the_quota_is_refused_mid_body() {
        let server = TestServer::start(100_000).await;
        let save = server.reserve(1).await;

        let refusal = server
            .put(&save, ONE_SHOT, vec![vec![7; 40_000]; 4])
            .await
            .expect_err("160 000 bytes into a 100 000 byte quota");

        /* The declared-size cap would have let all of this through: that is
           the hole this closes, so the refusal has to be the quota's. */
        assert!(size_limit(1) > 160_000);
        assert_eq!(refusal.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(refusal.message, QUOTA_MESSAGE);

        assert!(!save.path.exists(), "nothing was stored");
        assert!(!save.partial().exists(), "and no partial file was left");
        assert_eq!(server.row(&save.id).await, (0, 1), "the row never landed");
    }

    /// The honest case still lands, and what is recorded is the size that
    /// actually arrived — which is what the next upload is measured against.
    #[tokio::test]
    async fn an_upload_inside_the_quota_lands_and_records_its_real_size() {
        let server = TestServer::start(100_000).await;
        let save = server.reserve(50_000).await;

        let status = server
            .put(&save, ONE_SHOT, vec![vec![7; 25_000]; 2])
            .await
            .expect("50 000 bytes into a 100 000 byte quota");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            std::fs::metadata(&save.path).expect("the stored object").len(),
            50_000
        );
        assert!(!save.partial().exists());
        assert_eq!(server.row(&save.id).await, (1, 50_000));
    }

    /// A chunked upload that loses its room between chunks can never be
    /// finished, so the part already on disk goes with the refusal.
    #[tokio::test]
    async fn a_chunked_upload_refused_between_chunks_drops_its_partial_file() {
        let server = TestServer::start(100_000).await;
        let save = server.reserve(80_000).await;

        let status = server
            .put(
                &save,
                UploadQuery {
                    offset: Some(0),
                    total: Some(80_000),
                },
                vec![vec![7; 40_000]],
            )
            .await
            .expect("the first chunk fits");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            std::fs::metadata(save.partial()).expect("a partial file").len(),
            40_000
        );

        /* The operator lowers the quota under what this upload reserved. */
        server.state.settings.write().await.max_bytes_per_user = 10_000;

        let refusal = server
            .put(
                &save,
                UploadQuery {
                    offset: Some(40_000),
                    total: Some(80_000),
                },
                vec![vec![7; 40_000]],
            )
            .await
            .expect_err("the room it reserved is gone");

        assert_eq!(refusal.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(refusal.message, QUOTA_MESSAGE);
        assert!(
            !save.partial().exists(),
            "the half an upload that can never be finished"
        );
        assert!(!save.path.exists());
    }

    /// Nothing above should cost anything on a server that has no quota: the
    /// gate never opens, so it never reads the database.
    #[tokio::test]
    async fn an_unlimited_server_stores_what_it_is_given() {
        let server = TestServer::start(0).await;
        let save = server.reserve(1).await;

        let status = server
            .put(&save, ONE_SHOT, vec![vec![7; 40_000]])
            .await
            .expect("no quota, no refusal");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(server.row(&save.id).await, (1, 40_000));
    }

}
