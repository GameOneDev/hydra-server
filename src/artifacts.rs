use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

/// Response shape matches the launcher's `GameArtifact` type. `shop` and
/// `object_id` are extra fields the launcher's Cloud Save Manager uses to
/// group a no-filter listing by game; `game_name`/`game_cover_url` come from
/// the `game_metadata` cache when the listing query joins it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameArtifact {
    pub id: String,
    pub artifact_length_in_bytes: i64,
    pub download_option_title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub hostname: String,
    pub download_count: i64,
    pub label: Option<String>,
    pub is_frozen: bool,
    pub shop: String,
    pub object_id: String,
    pub game_name: Option<String>,
    pub game_cover_url: Option<String>,
}

pub(crate) fn artifact_from_row(row: &sqlx::sqlite::SqliteRow) -> GameArtifact {
    GameArtifact {
        id: row.get("id"),
        artifact_length_in_bytes: row.get("artifact_length_in_bytes"),
        download_option_title: row.get("download_option_title"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        hostname: row.get("hostname"),
        download_count: row.get("download_count"),
        label: row.get("label"),
        is_frozen: row.get::<i64, _>("is_frozen") != 0,
        shop: row.get("shop"),
        object_id: row.get("object_id"),
        game_name: row.try_get("game_name").unwrap_or(None),
        game_cover_url: row.try_get("game_cover_url").unwrap_or(None),
    }
}

fn artifact_key(id: &str) -> String {
    format!("artifacts/{id}.tar")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    pub shop: Option<String>,
    pub object_id: Option<String>,
}

/// GET /profile/games/artifacts?shop=&objectId=
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<GameArtifact>>> {
    let rows = sqlx::query(
        "SELECT a.*, g.name AS game_name, g.cover_url AS game_cover_url
         FROM artifacts a
         LEFT JOIN game_metadata g ON g.shop = a.shop AND g.object_id = a.object_id
         WHERE a.user_id = ?
           AND a.is_uploaded = 1
           AND (? IS NULL OR a.shop = ?)
           AND (? IS NULL OR a.object_id = ?)
         ORDER BY a.created_at DESC",
    )
    .bind(&user.0.id)
    .bind(&query.shop)
    .bind(&query.shop)
    .bind(&query.object_id)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(rows.iter().map(artifact_from_row).collect()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateArtifact {
    pub artifact_length_in_bytes: i64,
    pub shop: String,
    pub object_id: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub wine_prefix_path: Option<String>,
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default)]
    pub download_option_title: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// POST /profile/games/artifacts -> { id, uploadUrl }
pub async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<CreateArtifact>,
) -> ApiResult<Json<serde_json::Value>> {
    /* Zero is as invalid as negative: it is the size the upload token is
       bound to and the number the quota below is checked against. */
    let limit = storage::upload_limit(payload.artifact_length_in_bytes)
        .ok_or_else(|| ApiError::bad_request("invalid artifact length"))?;

    enforce_quotas(&state, &user.0.id, &payload).await?;

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO artifacts (
            id, user_id, shop, object_id, artifact_length_in_bytes, hostname,
            wine_prefix_path, home_dir, download_option_title, platform, label,
            created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user.0.id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .bind(payload.artifact_length_in_bytes)
    .bind(payload.hostname.as_deref().unwrap_or(""))
    .bind(&payload.wine_prefix_path)
    .bind(payload.home_dir.as_deref().unwrap_or(""))
    .bind(&payload.download_option_title)
    .bind(&payload.platform)
    .bind(&payload.label)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    let upload_url = storage::sign_upload_url(&state, &artifact_key(&id), limit);

    crate::events::record(
        &state,
        Event::sync(
            "backup.created",
            &user.0.id,
            match payload.label.as_deref() {
                Some(label) => format!("Uploaded a save backup — {label}"),
                None => "Uploaded a save backup".to_string(),
            },
        )
        .game(&payload.shop, &payload.object_id)
        .detail(json!({ "artifactId": id, "hostname": payload.hostname }))
        .size(payload.artifact_length_in_bytes),
    )
    .await;

    Ok(Json(json!({ "id": id, "uploadUrl": upload_url })))
}

async fn enforce_quotas(
    state: &AppState,
    user_id: &str,
    payload: &CreateArtifact,
) -> ApiResult<()> {
    let backups_per_game_limit = crate::limits::for_user(state, user_id)
        .await?
        .backups_per_game_limit;

    let per_game: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifacts WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .fetch_one(&state.pool)
    .await?;

    if per_game >= backups_per_game_limit as i64 {
        return Err(ApiError::bad_request(
            "backup limit for this game reached — delete an older backup first",
        ));
    }

    /* The declared length, which is all there is to go on before the upload:
       `storage::upload` holds the bytes to what is actually left. */
    storage::check_quota(state, user_id, payload.artifact_length_in_bytes).await?;

    Ok(())
}

/// POST /profile/games/artifacts/{id}/download
/// -> { downloadUrl, objectKey, homeDir, winePrefixPath }
///
/// The owner can always download; so can any user the backup was shared with.
pub async fn download(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = sqlx::query(
        "SELECT * FROM artifacts
         WHERE id = ?
           AND is_uploaded = 1
           AND (
             user_id = ?
             OR EXISTS (
               SELECT 1 FROM artifact_shares
               WHERE artifact_id = artifacts.id AND recipient_user_id = ?
             )
           )",
    )
    .bind(&id)
    .bind(&user.0.id)
    .bind(&user.0.id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("artifact not found"))?;

    sqlx::query("UPDATE artifacts SET download_count = download_count + 1 WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    let download_url = storage::sign_download_url(&state, &artifact_key(&id));

    Ok(Json(json!({
        "downloadUrl": download_url,
        /* The launcher joins objectKey onto its userData dir as a temp file
           name, so keep it flat. */
        "objectKey": format!("hydra-artifact-{id}.tar"),
        "homeDir": row.get::<String, _>("home_dir"),
        "winePrefixPath": row.get::<Option<String>, _>("wine_prefix_path"),
    })))
}

/// DELETE /profile/games/artifacts/{id}
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query("DELETE FROM artifacts WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(&user.0.id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    /* Foreign keys are not enforced on this connection, so drop the share
       rows explicitly. */
    sqlx::query("DELETE FROM artifact_shares WHERE artifact_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    storage::delete_object(&state, &artifact_key(&id)).await;

    Ok(StatusCode::OK)
}

/// PUT /profile/games/artifacts/{id}/freeze | /unfreeze
pub async fn set_frozen(
    state: AppState,
    user: CurrentUser,
    id: String,
    frozen: bool,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE artifacts SET is_frozen = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(frozen as i64)
    .bind(Utc::now().to_rfc3339())
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    Ok(StatusCode::OK)
}

pub async fn freeze(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    set_frozen(state, user, id, true).await
}

pub async fn unfreeze(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    set_frozen(state, user, id, false).await
}

#[derive(Deserialize)]
pub struct RenameArtifact {
    pub label: Option<String>,
}

/// PUT|PATCH /profile/games/artifacts/{id} — rename a backup.
///
/// The launcher's rename modal sends PUT; PATCH answers the same way for
/// clients that send it. See the route in `main.rs`.
pub async fn rename(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
    Json(payload): Json<RenameArtifact>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE artifacts SET label = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(&payload.label)
    .bind(Utc::now().to_rfc3339())
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("artifact not found"));
    }

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CachedUser;
    use crate::testing::TestServer;

    const TOKEN: &str = "alices-access-token";

    /// One uploaded backup of alice's — the row the rename modal acts on.
    async fn backup(server: &TestServer) {
        sqlx::query(
            "INSERT INTO artifacts
               (id, user_id, shop, object_id, artifact_length_in_bytes, label,
                is_uploaded, created_at, updated_at)
             VALUES ('backup-1', 'alice', 'steam', '440', 64, 'Before the boss',
                     1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&server.state.pool)
        .await
        .expect("a backup to rename");
    }

    /// A token the auth extractor accepts. Seeding the cache is what the first
    /// verified request would leave behind, and keeps the test off the
    /// official API.
    async fn authorize(server: &TestServer) {
        server.state.token_cache.write().await.insert(
            TOKEN.to_string(),
            CachedUser {
                user: server.user("alice").0,
                cached_at: Utc::now(),
            },
        );
    }

    /// The assembled router, on a real loopback port.
    ///
    /// Calling `rename` directly would pass whatever methods the route is
    /// registered under — and the method is the whole bug: the launcher's PUT
    /// was rejected by the router, before any handler ran. Only a real request
    /// over `crate::router` can tell 405 from 200.
    async fn serve(server: &TestServer) -> String {
        let app = crate::router(server.state.clone()).with_state(server.state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let base = format!("http://{}", listener.local_addr().expect("the bound address"));

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("the test server");
        });

        base
    }

    async fn rename_over_http(server: &TestServer, method: reqwest::Method) -> StatusCode {
        let base = serve(server).await;

        /* The proxy this may run behind has no business intercepting loopback. */
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an http client");

        let response = client
            .request(method, format!("{base}/profile/games/artifacts/backup-1"))
            .bearer_auth(TOKEN)
            .json(&json!({ "label": "After the boss" }))
            .send()
            .await
            .expect("a response");

        StatusCode::from_u16(response.status().as_u16()).expect("a known status")
    }

    async fn label(server: &TestServer) -> String {
        server
            .scalar::<String>("SELECT label FROM artifacts WHERE id = 'backup-1'")
            .await
    }

    /// The rename modal sends PUT — upstream's own call, which the official API
    /// answers. Registering only PATCH here made every self-hosted rename a 405.
    #[tokio::test]
    async fn the_launchers_rename_put_is_answered() {
        let server = TestServer::start().await;
        authorize(&server).await;
        backup(&server).await;

        assert_eq!(
            rename_over_http(&server, reqwest::Method::PUT).await,
            StatusCode::OK
        );
        assert_eq!(label(&server).await, "After the boss");
    }

    /// And PATCH keeps working, for anything already sending it.
    #[tokio::test]
    async fn a_rename_patch_is_answered_too() {
        let server = TestServer::start().await;
        authorize(&server).await;
        backup(&server).await;

        assert_eq!(
            rename_over_http(&server, reqwest::Method::PATCH).await,
            StatusCode::OK
        );
        assert_eq!(label(&server).await, "After the boss");
    }
}
