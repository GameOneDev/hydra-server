use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const MAX_IMAGE_BYTES: i64 = 30 * 1024 * 1024;

const ALLOWED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "apng", "gif", "webp"];

fn mime_for_extension(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "apng" => "image/apng",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresignRequest {
    pub image_ext: String,
    #[serde(default)]
    pub image_length: Option<i64>,
}

/// POST /presigned-urls/{profile-image|background-image}
///
/// Mirrors the official endpoint the launcher uses when changing profile
/// images: returns a presigned PUT URL plus the final public URL, which the
/// launcher then saves to the official profile via PATCH /profile. The image
/// itself is stored and served by this server, so it works without a Hydra
/// Cloud subscription.
pub async fn presign(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(image_type): Path<String>,
    Json(payload): Json<PresignRequest>,
) -> ApiResult<Json<Value>> {
    let url_field = match image_type.as_str() {
        "background-image" => "backgroundImageUrl",
        "profile-image" => "profileImageUrl",
        _ => return Err(ApiError::not_found("unknown image type")),
    };

    let ext = payload.image_ext.trim_start_matches('.').to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(ApiError::bad_request("unsupported image format"));
    }

    let length = payload.image_length.unwrap_or(0);
    if length > MAX_IMAGE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "image is too large",
        ));
    }

    let kind = if image_type == "background-image" {
        "banners"
    } else {
        "avatars"
    };

    let file_name = format!("{}.{ext}", Uuid::new_v4());
    let key = format!("images/{kind}/{}/{file_name}", user.0.id);

    let presigned_url = storage::sign_upload_url(&state, &key, length.max(0) as u64);
    let public_url = format!(
        "{}/images/{kind}/{}/{file_name}",
        state.config.public_url, user.0.id
    );

    Ok(Json(json!({
        "presignedUrl": presigned_url,
        url_field: public_url,
    })))
}

/// GET /images/{*path} — public, so profile banners/avatars saved to the
/// official profile render for everyone who views it.
pub async fn serve(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> ApiResult<Response> {
    if path.contains("..")
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        return Err(ApiError::bad_request("invalid path"));
    }

    let file_path = storage::storage_path(&state, &format!("images/{path}"));

    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|_| ApiError::not_found("image not found"))?;
    let length = file.metadata().await?.len();

    let ext = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");

    let stream = tokio_util::io::ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_for_extension(ext))
        .header(header::CONTENT_LENGTH, length)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal("failed to build response"))
}
