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
    /// max upload size in bytes (uploads only)
    pub max: u64,
    pub exp: i64,
}

/// Total bytes a user is storing here: save backups, emulation saves and
/// uploaded custom images. Everything the per-user quota is measured against
/// lives in one place so the quota check and the admin panel can't drift.
pub async fn used_bytes(state: &AppState, user_id: &str) -> ApiResult<i64> {
    let used: i64 = sqlx::query_scalar(
        "SELECT (SELECT COALESCE(SUM(artifact_length_in_bytes), 0)
                   FROM artifacts WHERE user_id = ?1)
              + (SELECT COALESCE(SUM(artifact_length_in_bytes), 0)
                   FROM emulation_saves WHERE user_id = ?1)
              + (SELECT COALESCE(SUM(size_in_bytes), 0)
                   FROM game_artwork WHERE user_id = ?1)",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;

    Ok(used)
}

pub fn sign_upload_url(state: &AppState, key: &str, max_bytes: u64) -> String {
    sign_url(state, "put", key, max_bytes, UPLOAD_TOKEN_TTL_SECONDS)
}

pub fn sign_download_url(state: &AppState, key: &str) -> String {
    sign_url(state, "get", key, 0, DOWNLOAD_TOKEN_TTL_SECONDS)
}

fn sign_url(state: &AppState, op: &str, key: &str, max: u64, ttl: i64) -> String {
    let claims = StorageClaims {
        op: op.to_string(),
        key: key.to_string(),
        max,
        exp: Utc::now().timestamp() + ttl,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.secret.as_bytes()),
    )
    .expect("failed to sign storage token");

    format!("{}/storage/{}", state.config.public_url, token)
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

    let size_limit = if claims.max > 0 {
        /* `max` is the size the launcher declared when it created the
           artifact; a little slack covers metadata drift between stat()
           and the actual upload. */
        Some(claims.max + (claims.max / 10) + 1024 * 1024)
    } else {
        None
    };

    if let (Some(limit), Some(total)) = (size_limit, total) {
        if total > limit {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds declared size",
            ));
        }
    }

    let temp_path = path.with_extension("uploading");

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

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| ApiError::bad_request("upload interrupted"))?;

        written += chunk.len() as u64;

        let over_declared_size =
            size_limit.is_some_and(|limit| written > limit);
        let over_declared_total = total.is_some_and(|total| written > total);

        if over_declared_size || over_declared_total {
            drop(file);
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload exceeds declared size",
            ));
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

    tokio::fs::rename(&temp_path, &path).await?;

    finalize_upload(&state, &claims.key, written).await?;

    tracing::info!("stored {} ({} bytes)", claims.key, written);
    Ok(StatusCode::OK)
}

/// Marks the owning row as uploaded once the bytes are on disk.
async fn finalize_upload(state: &AppState, key: &str, written: u64) -> ApiResult<()> {
    let now = Utc::now().to_rfc3339();

    /* Banner uploads become the user's current banner; the previous file is
       deleted so banners don't accumulate. */
    if let Some(rest) = key.strip_prefix("images/banners/") {
        if let Some((user_id, _file)) = rest.split_once('/') {
            let old_key: Option<String> =
                sqlx::query_scalar("SELECT banner_key FROM users WHERE id = ?")
                    .bind(user_id)
                    .fetch_optional(&state.pool)
                    .await?
                    .flatten();

            sqlx::query("UPDATE users SET banner_key = ? WHERE id = ?")
                .bind(key)
                .bind(user_id)
                .execute(&state.pool)
                .await?;

            if let Some(old_key) = old_key {
                if old_key != key {
                    delete_object(state, &old_key).await;
                }
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

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal("failed to build response"))?;

    Ok(response)
}
