use crate::error::ApiError;
use crate::state::{AppState, AuthenticatedUser, CachedUser};
use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use chrono::Utc;
use serde::Deserialize;

const TOKEN_CACHE_TTL_SECONDS: i64 = 300;

/// The launcher authenticates with its OFFICIAL Hydra access token; this
/// server never issues credentials of its own. The token is validated by
/// calling the official `/profile/me` endpoint, which both proves the token
/// is genuine and tells us who the user is. Accounts, friends and the rest
/// of Hydra keep working exactly as before.
pub struct CurrentUser(pub AuthenticatedUser);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfficialProfile {
    id: String,
    username: Option<String>,
    display_name: Option<String>,
    profile_image_url: Option<String>,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string)
            .ok_or_else(|| ApiError::unauthorized("missing access token"))?;

        if token.is_empty() {
            return Err(ApiError::unauthorized("missing access token"));
        }

        let user = resolve_user(state, &token).await?;

        let allowed = state
            .settings
            .read()
            .await
            .user_allowed(&user.id, user.username.as_deref());
        if !allowed {
            return Err(ApiError::forbidden("user not allowed on this server"));
        }

        let blocked: Option<(i64,)> =
            sqlx::query_as("SELECT is_blocked FROM users WHERE id = ?")
                .bind(&user.id)
                .fetch_optional(&state.pool)
                .await?;

        if matches!(blocked, Some((1,))) {
            return Err(ApiError::forbidden("user is blocked on this server"));
        }

        Ok(CurrentUser(user))
    }
}

async fn resolve_user(state: &AppState, token: &str) -> Result<AuthenticatedUser, ApiError> {
    {
        let cache = state.token_cache.read().await;
        if let Some(cached) = cache.get(token) {
            let age = Utc::now()
                .signed_duration_since(cached.cached_at)
                .num_seconds();
            if age < TOKEN_CACHE_TTL_SECONDS {
                return Ok(cached.user.clone());
            }
        }
    }

    let user = verify_with_official_api(state, token).await?;
    upsert_user(state, &user).await?;

    let mut cache = state.token_cache.write().await;
    cache.retain(|_, cached| {
        Utc::now()
            .signed_duration_since(cached.cached_at)
            .num_seconds()
            < TOKEN_CACHE_TTL_SECONDS
    });
    cache.insert(
        token.to_string(),
        CachedUser {
            user: user.clone(),
            cached_at: Utc::now(),
        },
    );

    Ok(user)
}

async fn verify_with_official_api(
    state: &AppState,
    token: &str,
) -> Result<AuthenticatedUser, ApiError> {
    let url = format!("{}/profile/me", state.config.official_api_url);

    let response = state
        .http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| {
            tracing::warn!("official API unreachable: {err}");
            /* Anything but a real 401 must NOT look like one — the launcher
               wipes its session on 401 responses. */
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "official Hydra API unreachable",
            )
        })?;

    match response.status() {
        status if status.is_success() => {}
        StatusCode::UNAUTHORIZED => {
            return Err(ApiError::unauthorized("invalid access token"));
        }
        status => {
            tracing::warn!("official API returned {status} while validating a token");
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "official Hydra API error",
            ));
        }
    }

    let profile: OfficialProfile = response
        .json()
        .await
        .map_err(|_| ApiError::internal("unexpected official API response"))?;

    Ok(AuthenticatedUser {
        display_name: profile.display_name.unwrap_or_else(|| profile.id.clone()),
        id: profile.id,
        username: profile.username,
        profile_image_url: profile.profile_image_url,
    })
}

async fn upsert_user(state: &AppState, user: &AuthenticatedUser) -> Result<(), ApiError> {
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO users (id, username, display_name, profile_image_url, created_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           username = excluded.username,
           display_name = excluded.display_name,
           profile_image_url = excluded.profile_image_url,
           last_seen_at = excluded.last_seen_at",
    )
    .bind(&user.id)
    .bind(&user.username)
    .bind(&user.display_name)
    .bind(&user.profile_image_url)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    Ok(())
}
