//! Portal sign-in: official credentials in, a cookie session of our own out.

use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::ratelimit;
use crate::state::{AppState, AuthenticatedUser};
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{header, request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;

const COOKIE_NAME: &str = "hydra_portal";
const SESSION_TTL_SECONDS: i64 = 60 * 60 * 24 * 7;
/// A link an operator hands to a user is meant to be used now, not kept.
const LINK_TTL_SECONDS: i64 = 15 * 60;
const SCOPE: &str = "portal";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/portal/api/login", post(login))
        .route("/portal/api/logout", post(logout))
        .route("/portal/api/session", get(session))
        .route("/portal/auth/{token}", get(link_sign_in))
}

#[derive(Serialize, Deserialize)]
struct PortalClaims {
    typ: String,
    sub: String,
    exp: i64,
}

/// The signed-in player. Every portal endpoint takes one, and every query
/// they make is scoped to `user_id`.
pub struct PortalSession {
    pub user_id: String,
}

impl FromRequestParts<AppState> for PortalSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if !state.config.portal_enabled {
            return Err(ApiError::forbidden("the portal is disabled on this server"));
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
            .ok_or_else(|| ApiError::unauthorized("sign in to continue"))?;

        let claims = decode::<PortalClaims>(
            &token,
            &DecodingKey::from_secret(state.config.secret.as_bytes()),
            &Validation::new(Algorithm::HS256),
        )
        .map_err(|_| ApiError::unauthorized("session expired"))?
        .claims;

        if claims.typ != "portal" {
            return Err(ApiError::unauthorized("invalid session"));
        }

        /* A block or a deletion has to take effect on the next request, not
        whenever the week-long cookie happens to expire. */
        let row: Option<(i64,)> = sqlx::query_as("SELECT is_blocked FROM users WHERE id = ?")
            .bind(&claims.sub)
            .fetch_optional(&state.pool)
            .await?;

        match row {
            Some((0,)) => Ok(PortalSession {
                user_id: claims.sub,
            }),
            Some(_) => Err(ApiError::forbidden(
                "this account is blocked on this server",
            )),
            None => Err(ApiError::unauthorized("this account no longer exists")),
        }
    }
}

fn session_cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; HttpOnly; Path=/; Max-Age={SESSION_TTL_SECONDS}; SameSite=Lax")
}

fn issue(state: &AppState, user_id: &str, ttl: i64, typ: &str) -> ApiResult<String> {
    encode(
        &Header::default(),
        &PortalClaims {
            typ: typ.to_string(),
            sub: user_id.to_string(),
            exp: Utc::now().timestamp() + ttl,
        },
        &EncodingKey::from_secret(state.config.secret.as_bytes()),
    )
    .map_err(|_| ApiError::internal("failed to create session"))
}

/// A short-lived URL that signs one specific user in. Minted by the admin
/// panel, never by the portal itself.
pub fn issue_link_token(state: &AppState, user_id: &str) -> ApiResult<(String, i64)> {
    let token = issue(state, user_id, LINK_TTL_SECONDS, "portal-link")?;
    Ok((token, LINK_TTL_SECONDS))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// For anyone who already has a launcher access token to hand.
    #[serde(default)]
    access_token: Option<String>,
}

/// POST /portal/api/login
async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    if !state.config.portal_enabled {
        return Err(ApiError::forbidden("the portal is disabled on this server"));
    }

    let ip = crate::client_ip::of(&state.config, &headers, Some(peer));
    ratelimit::check(&state, SCOPE, &ip).await?;

    let token = match (&request.access_token, &request.email, &request.password) {
        (Some(token), _, _) if !token.trim().is_empty() => token.trim().to_string(),
        (_, Some(email), Some(password)) if !email.is_empty() && !password.is_empty() => {
            match exchange_credentials(&state, email, password).await {
                Ok(token) => token,
                Err(error) => return Err(fail(&state, &ip, error).await),
            }
        }
        _ => return Err(ApiError::bad_request("enter your email and password")),
    };

    /* The official API is the only authority on who this is — the same call
    the launcher's token goes through on every sync. */
    let user = match crate::auth::verify_token(&state, &token).await {
        Ok(user) => user,
        Err(error) => {
            let message = if error.status == StatusCode::UNAUTHORIZED {
                ApiError::unauthorized("that sign-in didn't work")
            } else {
                error
            };
            return Err(fail(&state, &ip, message).await);
        }
    };

    let allowed = state
        .settings
        .read()
        .await
        .user_allowed(&user.id, user.username.as_deref());
    if !allowed {
        return Err(ApiError::forbidden(
            "this account isn't allowed on this server",
        ));
    }

    crate::auth::upsert_user(&state, &user).await?;

    /* Signing in to the portal is being seen, and upsert_user deliberately
    leaves that column to whoever knows it happened. */
    let blocked: Option<(i64,)> =
        sqlx::query_as("UPDATE users SET last_seen_at = ? WHERE id = ? RETURNING is_blocked")
            .bind(Utc::now().to_rfc3339())
            .bind(&user.id)
            .fetch_optional(&state.pool)
            .await?;
    if matches!(blocked, Some((1,))) {
        return Err(ApiError::forbidden(
            "this account is blocked on this server",
        ));
    }

    ratelimit::record_success(&state, SCOPE, &ip).await;
    finish_sign_in(&state, &user, &ip, "password").await
}

/// Records the miss, counts it, and hands back the error to return.
async fn fail(state: &AppState, ip: &str, error: ApiError) -> ApiError {
    use std::sync::atomic::Ordering;
    state.metrics.login_failures.fetch_add(1, Ordering::Relaxed);

    let (failures, locked) = ratelimit::record_failure(state, SCOPE, ip).await;

    crate::events::record(
        state,
        Event::auth("auth.portal.failed", "Failed portal sign-in")
            .actor("anonymous")
            .ip(Some(ip.to_string()))
            .detail(json!({ "failures": failures, "lockedOut": locked.is_some() }))
            .warning(),
    )
    .await;

    match locked {
        Some(_) => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed attempts — try again later",
        ),
        None => error,
    }
}

async fn finish_sign_in(
    state: &AppState,
    user: &AuthenticatedUser,
    ip: &str,
    method: &str,
) -> ApiResult<Response> {
    let token = issue(state, &user.id, SESSION_TTL_SECONDS, "portal")?;

    crate::events::record(
        state,
        Event::auth(
            "auth.portal.login",
            format!("{} signed in to the portal", user.display_name),
        )
        .actor(format!("user:{}", user.id))
        .about(&user.id)
        .ip(Some(ip.to_string()))
        .detail(json!({ "method": method })),
    )
    .await;

    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

/// Posts the sign-in form to the official API and pulls the access token out
/// of whatever shape it answers with.
///
/// The credentials are used here and nowhere else — not logged, not stored,
/// not kept in memory past this call.
async fn exchange_credentials(
    state: &AppState,
    email: &str,
    password: &str,
) -> Result<String, ApiError> {
    let url = format!(
        "{}{}",
        state.config.official_api_url, state.config.official_login_path
    );

    let response = state
        .http
        .post(&url)
        .json(&json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|err| {
            tracing::warn!("official API unreachable during portal sign-in: {err}");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "can't reach the official Hydra API right now",
            )
        })?;

    let status = response.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
        /* This deployment's official API has no password endpoint. Say so
        precisely — the operator can hand out portal links instead. */
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "this server's Hydra API doesn't accept password sign-in — ask the server admin for a portal link",
        ));
    }
    if !status.is_success() {
        return Err(ApiError::unauthorized("wrong email or password"));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|_| ApiError::internal("unexpected response from the official API"))?;

    extract_access_token(&body).ok_or_else(|| {
        ApiError::internal("the official API accepted the sign-in but returned no access token")
    })
}

/// Pulls the access token out of a login response.
///
/// Different Hydra builds nest it differently, and a self-hosted server may be
/// pointed at any of them, so this looks where it has been known to live
/// rather than demanding one exact shape.
fn extract_access_token(body: &Value) -> Option<String> {
    const PATHS: [&[&str]; 6] = [
        &["accessToken"],
        &["access_token"],
        &["token"],
        &["data", "accessToken"],
        &["tokens", "accessToken"],
        &["result", "accessToken"],
    ];

    PATHS.iter().find_map(|path| {
        let mut node = body;
        for key in *path {
            node = node.get(key)?;
        }
        node.as_str()
            .filter(|token| !token.is_empty())
            .map(str::to_string)
    })
}

/// GET /portal/auth/{token} — opens a portal link and lands on the portal.
async fn link_sign_in(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let claims = decode::<PortalClaims>(
        &token,
        &DecodingKey::from_secret(state.config.secret.as_bytes()),
        &Validation::new(Algorithm::HS256),
    )
    .map_err(|_| ApiError::unauthorized("this link has expired — ask for a new one"))?
    .claims;

    if claims.typ != "portal-link" {
        return Err(ApiError::unauthorized("invalid link"));
    }

    let user: Option<(String, String)> =
        sqlx::query_as("SELECT id, display_name FROM users WHERE id = ? AND is_blocked = 0")
            .bind(&claims.sub)
            .fetch_optional(&state.pool)
            .await?;
    let (id, display_name) = user.ok_or_else(|| ApiError::unauthorized("account unavailable"))?;

    let session = issue(&state, &id, SESSION_TTL_SECONDS, "portal")?;

    crate::events::record(
        &state,
        Event::auth(
            "auth.portal.login",
            format!("{display_name} signed in with an operator link"),
        )
        .actor(format!("user:{id}"))
        .about(&id)
        .detail(json!({ "method": "link" })),
    )
    .await;

    Ok((
        [(header::SET_COOKIE, session_cookie(&session))],
        Redirect::to("/portal"),
    )
        .into_response())
}

async fn logout() -> Response {
    let cookie = format!("{COOKIE_NAME}=; HttpOnly; Path=/; Max-Age=0; SameSite=Lax");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

/// GET /portal/api/session — the boot probe, and the profile header.
async fn session(State(state): State<AppState>, portal: PortalSession) -> ApiResult<Json<Value>> {
    let row = sqlx::query_as::<_, (String, Option<String>, String, Option<String>)>(
        "SELECT id, username, display_name, profile_image_url FROM users WHERE id = ?",
    )
    .bind(&portal.user_id)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(json!({
        "authenticated": true,
        "user": {
            "id": row.0,
            "username": row.1,
            "displayName": row.2,
            "profileImageUrl": row.3,
        },
        "server": {
            "publicUrl": state.config.public_url,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The portal is pointed at whatever official API the operator configured,
    /// so the token hunt has to survive more than one response shape.
    #[test]
    fn access_tokens_are_found_wherever_they_are_nested() {
        assert_eq!(
            extract_access_token(&json!({ "accessToken": "abc" })).unwrap(),
            "abc"
        );
        assert_eq!(
            extract_access_token(&json!({ "access_token": "abc" })).unwrap(),
            "abc"
        );
        assert_eq!(
            extract_access_token(&json!({ "data": { "accessToken": "abc" } })).unwrap(),
            "abc"
        );
        assert_eq!(
            extract_access_token(
                &json!({ "tokens": { "accessToken": "abc", "refreshToken": "r" } })
            )
            .unwrap(),
            "abc"
        );
    }

    #[test]
    fn a_response_without_a_token_is_not_a_sign_in() {
        assert!(extract_access_token(&json!({ "ok": true })).is_none());
        assert!(extract_access_token(&json!({ "accessToken": "" })).is_none());
        /* A non-string token is a shape we don't understand, not a token. */
        assert!(extract_access_token(&json!({ "accessToken": 42 })).is_none());
        assert!(extract_access_token(&json!({ "data": {} })).is_none());
    }
}
