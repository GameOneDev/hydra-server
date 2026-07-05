use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::storage;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::{header, request::Parts};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;

const SESSION_TTL_SECONDS: i64 = 60 * 60 * 12;
const COOKIE_NAME: &str = "hydra_admin";

#[derive(Serialize, Deserialize)]
struct AdminClaims {
    typ: String,
    exp: i64,
}

/// Extractor guarding every admin endpoint with the session cookie.
pub struct AdminSession;

impl FromRequestParts<AppState> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if state.config.admin_password.is_empty() {
            return Err(ApiError::forbidden(
                "admin panel disabled — set HYDRA_ADMIN_PASSWORD",
            ));
        }

        let token = parts
            .headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|cookies| {
                cookies.split(';').find_map(|cookie| {
                    cookie
                        .trim()
                        .strip_prefix(&format!("{COOKIE_NAME}="))
                        .map(str::to_string)
                })
            })
            .ok_or_else(|| ApiError::unauthorized("admin login required"))?;

        let claims = decode::<AdminClaims>(
            &token,
            &DecodingKey::from_secret(state.config.secret.as_bytes()),
            &Validation::new(Algorithm::HS256),
        )
        .map_err(|_| ApiError::unauthorized("admin session expired"))?
        .claims;

        if claims.typ != "admin" {
            return Err(ApiError::unauthorized("invalid admin session"));
        }

        Ok(AdminSession)
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(index))
        .route("/admin/api/login", post(login))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/users", get(list_users))
        .route("/admin/api/users/{id}", get(user_details).delete(delete_user))
        .route("/admin/api/users/{id}/block", post(set_blocked))
        .route("/admin/api/artifacts/{id}", delete(delete_artifact))
        .route("/admin/api/artifacts/{id}/download", get(download_artifact))
        .route("/admin/api/emulation-saves/{id}", delete(delete_emulation_save))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/admin.html"))
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> ApiResult<Response> {
    if state.config.admin_password.is_empty() {
        return Err(ApiError::forbidden(
            "admin panel disabled — set HYDRA_ADMIN_PASSWORD",
        ));
    }

    /* constant-time-ish comparison to avoid trivially timing the password */
    let expected = state.config.admin_password.as_bytes();
    let given = payload.password.as_bytes();
    let matches = expected.len() == given.len()
        && expected
            .iter()
            .zip(given)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;

    if !matches {
        return Err(ApiError::unauthorized("wrong password"));
    }

    let token = encode(
        &Header::default(),
        &AdminClaims {
            typ: "admin".to_string(),
            exp: Utc::now().timestamp() + SESSION_TTL_SECONDS,
        },
        &EncodingKey::from_secret(state.config.secret.as_bytes()),
    )
    .map_err(|_| ApiError::internal("failed to create session"))?;

    let cookie =
        format!("{COOKIE_NAME}={token}; HttpOnly; Path=/; Max-Age={SESSION_TTL_SECONDS}; SameSite=Strict");

    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response())
}

async fn logout() -> Response {
    let cookie = format!("{COOKIE_NAME}=; HttpOnly; Path=/; Max-Age=0; SameSite=Strict");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn overview(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await?;
    let artifact_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
        .fetch_one(&state.pool)
        .await?;
    let artifact_bytes: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts")
            .fetch_one(&state.pool)
            .await?;
    let save_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM emulation_saves")
        .fetch_one(&state.pool)
        .await?;
    let save_bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves",
    )
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({
        "userCount": user_count,
        "artifactCount": artifact_count,
        "emulationSaveCount": save_count,
        "totalBytes": artifact_bytes + save_bytes,
        "maxBytesPerUser": state.config.max_bytes_per_user,
        "backupsPerGameLimit": state.config.backups_per_game_limit,
        "officialApiUrl": state.config.official_api_url,
        "publicUrl": state.config.public_url,
    })))
}

async fn list_users(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT u.*,
            (SELECT COUNT(*) FROM artifacts a WHERE a.user_id = u.id) AS artifact_count,
            (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts a WHERE a.user_id = u.id)
              + (SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves e WHERE e.user_id = u.id)
              AS total_bytes,
            (SELECT COUNT(*) FROM emulation_saves e WHERE e.user_id = u.id) AS save_count,
            (SELECT COUNT(*) FROM game_achievements g WHERE g.user_id = u.id) AS achievement_games
         FROM users u ORDER BY u.last_seen_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    let users: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "username": row.get::<Option<String>, _>("username"),
                "displayName": row.get::<String, _>("display_name"),
                "profileImageUrl": row.get::<Option<String>, _>("profile_image_url"),
                "isBlocked": row.get::<i64, _>("is_blocked") != 0,
                "lastSeenAt": row.get::<String, _>("last_seen_at"),
                "createdAt": row.get::<String, _>("created_at"),
                "artifactCount": row.get::<i64, _>("artifact_count"),
                "emulationSaveCount": row.get::<i64, _>("save_count"),
                "achievementGameCount": row.get::<i64, _>("achievement_games"),
                "totalBytes": row.get::<i64, _>("total_bytes"),
            })
        })
        .collect();

    Ok(Json(json!(users)))
}

async fn user_details(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let artifacts = sqlx::query(
        "SELECT * FROM artifacts WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    let saves = sqlx::query(
        "SELECT * FROM emulation_saves WHERE user_id = ? ORDER BY updated_at DESC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "artifacts": artifacts.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "shop": row.get::<String, _>("shop"),
            "objectId": row.get::<String, _>("object_id"),
            "label": row.get::<Option<String>, _>("label"),
            "sizeBytes": row.get::<i64, _>("artifact_length_in_bytes"),
            "hostname": row.get::<String, _>("hostname"),
            "platform": row.get::<Option<String>, _>("platform"),
            "isFrozen": row.get::<i64, _>("is_frozen") != 0,
            "isUploaded": row.get::<i64, _>("is_uploaded") != 0,
            "downloadCount": row.get::<i64, _>("download_count"),
            "createdAt": row.get::<String, _>("created_at"),
        })).collect::<Vec<_>>(),
        "emulationSaves": saves.iter().map(|row| json!({
            "id": row.get::<String, _>("id"),
            "platform": row.get::<String, _>("platform"),
            "emulator": row.get::<String, _>("emulator"),
            "fileName": row.get::<Option<String>, _>("file_name"),
            "label": row.get::<Option<String>, _>("label"),
            "sizeBytes": row.get::<i64, _>("artifact_length_in_bytes"),
            "isUploaded": row.get::<i64, _>("is_uploaded") != 0,
            "updatedAt": row.get::<String, _>("updated_at"),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct BlockRequest {
    blocked: bool,
}

async fn set_blocked(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(payload): Json<BlockRequest>,
) -> ApiResult<Json<Value>> {
    sqlx::query("UPDATE users SET is_blocked = ? WHERE id = ?")
        .bind(payload.blocked as i64)
        .bind(&id)
        .execute(&state.pool)
        .await?;

    /* Blocked users may still have a cached token — drop the cache so the
       block applies within seconds, not minutes. */
    state.token_cache.write().await.clear();

    Ok(Json(json!({ "ok": true })))
}

async fn delete_user(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let artifact_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM artifacts WHERE user_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;
    let save_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM emulation_saves WHERE user_id = ?")
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    for artifact_id in artifact_ids {
        storage::delete_object(&state, &format!("artifacts/{artifact_id}.tar")).await;
    }
    for save_id in save_ids {
        storage::delete_object(&state, &format!("emulation-saves/{save_id}.bin")).await;
    }

    state.token_cache.write().await.clear();

    Ok(Json(json!({ "ok": true })))
}

async fn delete_artifact(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = sqlx::query("DELETE FROM artifacts WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    storage::delete_object(&state, &format!("artifacts/{id}.tar")).await;

    Ok(Json(json!({ "ok": true })))
}

async fn download_artifact(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Redirect> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM artifacts WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;

    if exists.is_none() {
        return Err(ApiError::not_found("artifact not found"));
    }

    let url = storage::sign_download_url(&state, &format!("artifacts/{id}.tar"));
    Ok(Redirect::temporary(&url))
}

async fn delete_emulation_save(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let result = sqlx::query("DELETE FROM emulation_saves WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("emulation save not found"));
    }

    storage::delete_object(&state, &format!("emulation-saves/{id}.bin")).await;

    Ok(Json(json!({ "ok": true })))
}
