//! The settings the panel may change at runtime.
//!
//! Each one exists in three layers — the environment default, an optional
//! saved override, and the value in force — and the screen shows all three,
//! because "why is the quota 5 GB when the compose file says 20" is otherwise
//! an unanswerable question.

use super::AdminSession;
use crate::client_ip;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::settings as store;
use crate::state::{AppState, RuntimeSettings};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/admin/api/settings",
        get(get_settings).put(update_settings).delete(reset_settings),
    )
}

/// What this server makes of the request it is answering right now.
///
/// "Every sign-in is logged from the same address" is a proxy question, and
/// the only way to answer it is to compare what arrived with what was
/// believed — so the screen shows both, for the request that drew it.
fn proxy_report(state: &AppState, headers: &HeaderMap, peer: SocketAddr) -> Value {
    let resolved = client_ip::resolve(&state.config, headers, Some(peer));
    let observed = client_ip::observed_headers(headers);

    json!({
        "clientIp": resolved.ip,
        "source": resolved.source,
        "peer": peer.ip().to_string(),
        "trustProxyHeaders": state.config.trust_proxy_headers,
        "header": state.config.client_ip_header,
        "hops": state.config.trusted_proxy_hops,
        "observed": observed.iter().map(|(name, value)| json!({
            "name": name,
            "value": value,
        })).collect::<Vec<_>>(),
        /* The one combination that silently produces wrong addresses: a proxy
           is plainly in front, and the server was never told to believe it. */
        "ignoringHeaders": !state.config.trust_proxy_headers && !observed.is_empty(),
    })
}

async fn payload(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
) -> ApiResult<Json<Value>> {
    let current = state.settings.read().await.clone();
    let defaults = RuntimeSettings::from_config(&state.config);

    let overrides: Vec<(String, String, String)> =
        sqlx::query_as("SELECT key, value, updated_at FROM server_settings")
            .fetch_all(&state.pool)
            .await?;

    Ok(Json(json!({
        "current": {
            "maxBytesPerUser": current.max_bytes_per_user,
            "backupsPerGameLimit": current.backups_per_game_limit,
            "autoDeleteSaves": current.auto_delete_saves,
            "allowedUsers": current.allowed_users,
        },
        "defaults": {
            "maxBytesPerUser": defaults.max_bytes_per_user,
            "backupsPerGameLimit": defaults.backups_per_game_limit,
            "autoDeleteSaves": defaults.auto_delete_saves,
            "allowedUsers": defaults.allowed_users,
        },
        "overrides": overrides.iter().map(|(key, value, updated_at)| json!({
            "key": key,
            "value": value,
            "updatedAt": updated_at,
        })).collect::<Vec<_>>(),
        "environment": {
            "publicUrl": state.config.public_url,
            "officialApiUrl": state.config.official_api_url,
            "dataDir": state.config.data_dir.display().to_string(),
            "bind": state.config.bind,
        },
        "proxy": proxy_report(state, headers, peer),
    })))
}

async fn get_settings(
    State(state): State<AppState>,
    _admin: AdminSession,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    payload(&state, &headers, peer).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    max_bytes_per_user: Option<u64>,
    backups_per_game_limit: Option<u32>,
    auto_delete_saves: Option<bool>,
    allowed_users: Option<Vec<String>>,
}

/// PUT /admin/api/settings — persists the provided fields as overrides of the
/// environment defaults and applies them immediately.
async fn update_settings(
    State(state): State<AppState>,
    _admin: AdminSession,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<UpdateRequest>,
) -> ApiResult<Json<Value>> {
    if let Some(max_bytes) = request.max_bytes_per_user {
        store::set(&state.pool, store::MAX_BYTES_PER_USER, &max_bytes.to_string()).await?;
    }

    if let Some(limit) = request.backups_per_game_limit {
        if limit == 0 {
            return Err(ApiError::bad_request("backups per game must be at least 1"));
        }
        store::set(
            &state.pool,
            store::BACKUPS_PER_GAME_LIMIT,
            &limit.to_string(),
        )
        .await?;
    }

    if let Some(auto_delete) = request.auto_delete_saves {
        store::set(
            &state.pool,
            store::AUTO_DELETE_SAVES,
            if auto_delete { "true" } else { "false" },
        )
        .await?;
    }

    if let Some(users) = request.allowed_users {
        let normalized = store::parse_allowed_users(&users.join(","));
        store::set(&state.pool, store::ALLOWED_USERS, &normalized.join(",")).await?;
    }

    reload(&state).await;

    let current = state.settings.read().await.clone();
    crate::events::record(
        &state,
        Event::admin("admin.settings.updated", "Settings changed")
            .detail(json!({
                "maxBytesPerUser": current.max_bytes_per_user,
                "backupsPerGameLimit": current.backups_per_game_limit,
                "autoDeleteSaves": current.auto_delete_saves,
                "allowedUsers": current.allowed_users,
            })),
    )
    .await;

    payload(&state, &headers, peer).await
}

/// DELETE /admin/api/settings — clears every override, back to env values.
async fn reset_settings(
    State(state): State<AppState>,
    _admin: AdminSession,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    store::clear(&state.pool).await?;
    reload(&state).await;

    crate::events::record(
        &state,
        Event::admin("admin.settings.reset", "Settings reset to the environment values"),
    )
    .await;

    payload(&state, &headers, peer).await
}

async fn reload(state: &AppState) {
    let reloaded = store::load(&state.pool, &state.config).await;
    *state.settings.write().await = reloaded;
}
