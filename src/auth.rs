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

        /* Before the bump below overwrites the evidence: last_seen_at is how
        the presence log tells a returning client from a busy one. */
        let ip = crate::client_ip::of(
            &state.config,
            &parts.headers,
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
        );
        crate::presence::touch(state, &user, Some(ip)).await;

        /* Bump last_seen_at on every authenticated request. resolve_user only
        touches the row on token-cache misses, which would leave last_seen_at
        up to TOKEN_CACHE_TTL_SECONDS stale while the client is active. */
        let blocked: Option<(i64,)> =
            sqlx::query_as("UPDATE users SET last_seen_at = ? WHERE id = ? RETURNING is_blocked")
                .bind(Utc::now().to_rfc3339())
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

    let user = verify_token(state, token).await?;
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

/// Asks the official API who a token belongs to.
///
/// Public because the portal signs people in with credentials rather than a
/// header, and must reach the same verdict from the same authority.
pub async fn verify_token(state: &AppState, token: &str) -> Result<AuthenticatedUser, ApiError> {
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

/// Mirrors the official profile into the local users table.
pub async fn upsert_user(state: &AppState, user: &AuthenticatedUser) -> Result<(), ApiError> {
    let now = Utc::now().to_rfc3339();

    /* Deliberately does NOT move last_seen_at on an existing row: the presence
    log reads that column to tell a returning client from a busy one, and a
    write hidden in here would have overwritten the answer before it was
    asked. Every caller bumps it explicitly instead.

    created_at is only written by the insert, so getting it back and finding
    our own timestamp means this row is new — which is how a first sighting
    gets logged without a second query to ask. */
    let created_at: Option<(String,)> = sqlx::query_as(
        "INSERT INTO users (id, username, display_name, profile_image_url, created_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           username = excluded.username,
           display_name = excluded.display_name,
           profile_image_url = excluded.profile_image_url
         RETURNING created_at",
    )
    .bind(&user.id)
    .bind(&user.username)
    .bind(&user.display_name)
    .bind(&user.profile_image_url)
    .bind(&now)
    .bind(&now)
    .fetch_optional(&state.pool)
    .await?;

    if created_at.is_some_and(|(at,)| at == now) {
        crate::events::record(
            state,
            crate::events::Event::auth(
                "user.first_seen",
                format!("{} used this server for the first time", user.display_name),
            )
            .actor(format!("user:{}", user.id))
            .about(&user.id)
            .detail(serde_json::json!({
                "username": user.username,
                "displayName": user.display_name,
            })),
        )
        .await;
    }

    Ok(())
}
