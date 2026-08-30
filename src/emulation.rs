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
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A memory card, a battery save or a save state: kilobytes to a few dozen
/// megabytes even for the heaviest emulators. A declared size past this is a bug
/// or an abuse of the endpoint rather than a save. Note that `storage::upload`
/// enforces the signed limit with some slack, so the absolute uploaded size can be
/// slightly higher than this value. Whole-game save backups have their own endpoint,
/// which is where multi-gigabyte uploads belong.
const MAX_EMULATION_SAVE_BYTES: i64 = 512 * 1024 * 1024;

fn save_key(id: &str) -> String {
    format!("emulation-saves/{id}.bin")
}

/// Response shape matches the launcher's `EmulationCloudSave` type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmulationCloudSave {
    pub id: String,
    pub platform: String,
    pub emulator: String,
    pub save_kind: String,
    pub save_identity: String,
    pub artifact_length_in_bytes: i64,
    pub file_name: Option<String>,
    pub hostname: Option<String>,
    pub local_last_modified_at: Option<String>,
    pub label: Option<String>,
    pub metadata: Option<Value>,
    pub shop: Option<String>,
    pub object_id: Option<String>,
    pub last_uploaded_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn save_from_row(row: &sqlx::sqlite::SqliteRow) -> EmulationCloudSave {
    EmulationCloudSave {
        id: row.get("id"),
        platform: row.get("platform"),
        emulator: row.get("emulator"),
        save_kind: row.get("save_kind"),
        save_identity: row.get("save_identity"),
        artifact_length_in_bytes: row.get("artifact_length_in_bytes"),
        file_name: row.get("file_name"),
        hostname: row.get("hostname"),
        local_last_modified_at: row.get("local_last_modified_at"),
        label: row.get("label"),
        metadata: row
            .get::<Option<String>, _>("metadata")
            .and_then(|metadata| serde_json::from_str(&metadata).ok()),
        shop: row.get("shop"),
        object_id: row.get("object_id"),
        last_uploaded_at: row.get("last_uploaded_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

async fn fetch_save(
    state: &AppState,
    user_id: &str,
    id: &str,
) -> ApiResult<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM emulation_saves WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("emulation save not found"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    pub platform: Option<String>,
    pub emulator: Option<String>,
    pub save_kind: Option<String>,
    pub shop: Option<String>,
    pub object_id: Option<String>,
}

/// GET /profile/emulation-saves
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<EmulationCloudSave>>> {
    let rows = sqlx::query(
        "SELECT * FROM emulation_saves
         WHERE user_id = ?
           AND is_uploaded = 1
           AND superseded_at IS NULL
           AND (? IS NULL OR platform = ?)
           AND (? IS NULL OR emulator = ?)
           AND (? IS NULL OR save_kind = ?)
           AND (? IS NULL OR shop = ?)
           AND (? IS NULL OR object_id = ?)
         ORDER BY updated_at DESC",
    )
    .bind(&user.0.id)
    .bind(&query.platform)
    .bind(&query.platform)
    .bind(&query.emulator)
    .bind(&query.emulator)
    .bind(&query.save_kind)
    .bind(&query.save_kind)
    .bind(&query.shop)
    .bind(&query.shop)
    .bind(&query.object_id)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(rows.iter().map(save_from_row).collect()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUploadUrl {
    pub platform: String,
    pub emulator: String,
    #[serde(default = "default_save_kind")]
    pub save_kind: String,
    #[serde(default)]
    pub shop: Option<String>,
    #[serde(default)]
    pub object_id: Option<String>,
    pub save_identity: String,
    pub artifact_length_in_bytes: i64,
}

fn default_save_kind() -> String {
    "game_save".to_string()
}

/// POST /profile/emulation-saves/upload-url -> { id, uploadUrl }
pub async fn create_upload_url(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<CreateUploadUrl>,
) -> ApiResult<Json<Value>> {
    /* Before the insert, so a save that declares no size leaves no row: the
       upload token is bound to this number, and a zero used to mean "no
       limit". The launcher has the buffer in hand before it asks. */
    let limit = storage::upload_limit(payload.artifact_length_in_bytes)
        .ok_or_else(|| ApiError::bad_request("invalid artifact length"))?;

    if payload.artifact_length_in_bytes > MAX_EMULATION_SAVE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "emulation save is too large",
        ));
    }

    /* Against the declared length, like every other presign — the file isn't
       here yet. `storage::upload` re-checks against the bytes that arrive, so
       an understated length can't spend more than this reserves. */
    storage::check_quota(&state, &user.0.id, payload.artifact_length_in_bytes).await?;

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO emulation_saves (
            id, user_id, platform, emulator, save_kind, save_identity,
            artifact_length_in_bytes, shop, object_id, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user.0.id)
    .bind(&payload.platform)
    .bind(&payload.emulator)
    .bind(&payload.save_kind)
    .bind(&payload.save_identity)
    .bind(payload.artifact_length_in_bytes)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    let upload_url = storage::sign_upload_url(&state, &save_key(&id), limit);

    Ok(Json(json!({ "id": id, "uploadUrl": upload_url })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSave {
    #[serde(default)]
    pub artifact_length_in_bytes: Option<i64>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub local_last_modified_at: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// POST /profile/emulation-saves/{id}/commit -> EmulationCloudSave
pub async fn commit(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
    Json(payload): Json<CommitSave>,
) -> ApiResult<Json<EmulationCloudSave>> {
    fetch_save(&state, &user.0.id, &id).await?;

    let now = Utc::now().to_rfc3339();

    /* Replace older saves for the same slot: the launcher expects one save
       per saveIdentity, mirroring how a memory card slot works. */
    let old_rows = sqlx::query(
        "SELECT s.id, s.is_uploaded FROM emulation_saves s
         JOIN emulation_saves new_save ON new_save.id = ?
         WHERE s.user_id = ?
           AND s.id != new_save.id
           AND s.platform = new_save.platform
           AND s.emulator = new_save.emulator
           AND s.save_kind = new_save.save_kind
           AND s.save_identity = new_save.save_identity",
    )
    .bind(&id)
    .bind(&user.0.id)
    .fetch_all(&state.pool)
    .await?;

    let auto_delete = crate::limits::for_user(&state, &user.0.id)
        .await?
        .auto_delete_saves;

    let mut deleted = 0usize;
    let mut retained = 0usize;

    for old in &old_rows {
        let old_id: String = old.get("id");

        if auto_delete || old.get::<i64, _>("is_uploaded") == 0 {
            sqlx::query("DELETE FROM emulation_saves WHERE id = ?")
                .bind(&old_id)
                .execute(&state.pool)
                .await?;
            storage::delete_object(&state, &save_key(&old_id)).await;
            deleted += 1;
        } else {
            sqlx::query(
                "UPDATE emulation_saves SET superseded_at = COALESCE(superseded_at, ?)
                 WHERE id = ?",
            )
            .bind(&now)
            .bind(&old_id)
            .execute(&state.pool)
            .await?;
            retained += 1;
        }
    }

    sqlx::query(
        "UPDATE emulation_saves SET
            is_uploaded = 1,
            artifact_length_in_bytes = COALESCE(?, artifact_length_in_bytes),
            file_name = COALESCE(?, file_name),
            hostname = COALESCE(?, hostname),
            local_last_modified_at = COALESCE(?, local_last_modified_at),
            label = COALESCE(?, label),
            last_uploaded_at = ?,
            updated_at = ?
         WHERE id = ? AND user_id = ?",
    )
    .bind(payload.artifact_length_in_bytes)
    .bind(&payload.file_name)
    .bind(&payload.hostname)
    .bind(&payload.local_last_modified_at)
    .bind(&payload.label)
    .bind(&now)
    .bind(&now)
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    let row = fetch_save(&state, &user.0.id, &id).await?;

    crate::events::record(
        &state,
        Event::sync(
            "emulation_save.synced",
            &user.0.id,
            format!(
                "Synced an emulation save ({} · {})",
                row.get::<String, _>("emulator"),
                row.get::<String, _>("platform")
            ),
        )
        .detail(serde_json::json!({
            "saveId": id,
            "fileName": row.get::<Option<String>, _>("file_name"),
            "replaced": deleted,
            "retained": retained,
        }))
        .size(row.get::<i64, _>("artifact_length_in_bytes")),
    )
    .await;

    Ok(Json(save_from_row(&row)))
}

/// POST /profile/emulation-saves/{id}/download-url -> { downloadUrl }
pub async fn download_url(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row = fetch_save(&state, &user.0.id, &id).await?;

    if row.get::<i64, _>("is_uploaded") != 1 {
        return Err(ApiError::not_found("save has no uploaded content"));
    }

    Ok(Json(json!({
        "downloadUrl": storage::sign_download_url(&state, &save_key(&id)),
    })))
}

#[derive(Deserialize)]
pub struct UpdateSave {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// PUT /profile/emulation-saves/{id} -> EmulationCloudSave
pub async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
    Json(payload): Json<UpdateSave>,
) -> ApiResult<Json<EmulationCloudSave>> {
    fetch_save(&state, &user.0.id, &id).await?;

    let metadata_json = payload
        .metadata
        .as_ref()
        .map(|metadata| serde_json::to_string(metadata).unwrap_or_default());

    sqlx::query(
        "UPDATE emulation_saves SET
            label = COALESCE(?, label),
            metadata = COALESCE(?, metadata),
            updated_at = ?
         WHERE id = ? AND user_id = ?",
    )
    .bind(&payload.label)
    .bind(&metadata_json)
    .bind(Utc::now().to_rfc3339())
    .bind(&id)
    .bind(&user.0.id)
    .execute(&state.pool)
    .await?;

    let row = fetch_save(&state, &user.0.id, &id).await?;
    Ok(Json(save_from_row(&row)))
}

/// DELETE /profile/emulation-saves/{id}
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query("DELETE FROM emulation_saves WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(&user.0.id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("emulation save not found"));
    }

    storage::delete_object(&state, &save_key(&id)).await;

    Ok(StatusCode::OK)
}

/// Replacing a save in a slot is the one place the server deletes something
/// nobody asked it to, so both answers to "may it?" are pinned here.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Overrides;
    use crate::testing::TestServer;

    /// Two saves in one PCSX2 slot: `old`, already uploaded, and `new`,
    /// waiting to be committed over it.
    async fn slot(server: &TestServer) {
        for (id, uploaded) in [("old", 1), ("new", 0)] {
            sqlx::query(
                "INSERT INTO emulation_saves
                   (id, user_id, platform, emulator, save_kind, save_identity,
                    artifact_length_in_bytes, is_uploaded, created_at, updated_at)
                 VALUES (?, 'alice', 'ps2', 'pcsx2', 'game_save', 'slot-1', 64, ?,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(uploaded)
            .execute(&server.state.pool)
            .await
            .expect("a save in the slot");
        }

        let path = storage::storage_path(&server.state, &save_key("old"));
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the storage dir");
        std::fs::write(&path, [7u8; 64]).expect("the older save's bytes");
    }

    async fn commit_new(server: &TestServer) {
        let _ = commit(
            State(server.state.clone()),
            server.user("alice"),
            Path("new".to_string()),
            Json(CommitSave {
                artifact_length_in_bytes: Some(64),
                file_name: None,
                hostname: None,
                local_last_modified_at: None,
                label: None,
            }),
        )
        .await
        .expect("the commit");
    }

    async fn listed(server: &TestServer) -> Vec<String> {
        let Json(saves) = list(
            State(server.state.clone()),
            server.user("alice"),
            Query(ListQuery {
                platform: None,
                emulator: None,
                save_kind: None,
                shop: None,
                object_id: None,
            }),
        )
        .await
        .expect("the listing");

        saves.into_iter().map(|save| save.id).collect()
    }

    #[tokio::test]
    async fn the_slots_older_save_is_deleted_by_default() {
        let server = TestServer::start().await;
        slot(&server).await;

        let older = storage::storage_path(&server.state, &save_key("old"));
        commit_new(&server).await;

        assert_eq!(
            server.scalar::<i64>("SELECT COUNT(*) FROM emulation_saves").await,
            1
        );
        assert!(!older.exists(), "its bytes went with the row");
        assert_eq!(listed(&server).await, vec!["new".to_string()]);
    }

    /// Off, the older save keeps its row and its bytes — and stays out of the
    /// launcher's listing, which expects exactly one save per slot.
    #[tokio::test]
    async fn the_slots_older_save_is_kept_when_the_user_opts_out() {
        let server = TestServer::start().await;
        server
            .limits(
                "alice",
                Overrides {
                    auto_delete_saves: Some(false),
                    ..Default::default()
                },
            )
            .await;
        slot(&server).await;

        let older = storage::storage_path(&server.state, &save_key("old"));
        commit_new(&server).await;

        assert!(older.exists(), "the older save's bytes are still here");
        assert_eq!(
            server
                .scalar::<i64>(
                    "SELECT COUNT(*) FROM emulation_saves WHERE superseded_at IS NOT NULL"
                )
                .await,
            1
        );
        assert_eq!(listed(&server).await, vec!["new".to_string()]);

        assert_eq!(storage::used_bytes(&server.state, "alice").await.unwrap(), 128);
    }

    /// Keeping older versions is about saves, not about reservations: a row
    /// whose upload never arrived is charged to its owner and holds nothing,
    /// so the slot's next commit drops it whatever the setting says.
    #[tokio::test]
    async fn an_upload_that_never_arrived_is_never_kept() {
        let server = TestServer::start().await;
        server
            .limits(
                "alice",
                Overrides {
                    auto_delete_saves: Some(false),
                    ..Default::default()
                },
            )
            .await;
        slot(&server).await;
        server
            .execute("UPDATE emulation_saves SET is_uploaded = 0 WHERE id = 'old'")
            .await;

        commit_new(&server).await;

        assert_eq!(
            server.scalar::<i64>("SELECT COUNT(*) FROM emulation_saves").await,
            1,
            "the abandoned reservation went, and its declared bytes with it"
        );
        assert_eq!(storage::used_bytes(&server.state, "alice").await.unwrap(), 64);
    }

    /// Switching deletion back on clears what it left behind, on the next
    /// sync of that slot.
    #[tokio::test]
    async fn turning_deletion_back_on_clears_what_was_kept() {
        let server = TestServer::start().await;
        server
            .limits(
                "alice",
                Overrides {
                    auto_delete_saves: Some(false),
                    ..Default::default()
                },
            )
            .await;
        slot(&server).await;
        commit_new(&server).await;

        server.limits("alice", Overrides::default()).await;
        server
            .execute(
                "INSERT INTO emulation_saves
                   (id, user_id, platform, emulator, save_kind, save_identity,
                    artifact_length_in_bytes, is_uploaded, created_at, updated_at)
                 VALUES ('newer', 'alice', 'ps2', 'pcsx2', 'game_save', 'slot-1', 64, 0,
                         '2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z')",
            )
            .await;

        let _ = commit(
            State(server.state.clone()),
            server.user("alice"),
            Path("newer".to_string()),
            Json(CommitSave {
                artifact_length_in_bytes: Some(64),
                file_name: None,
                hostname: None,
                local_last_modified_at: None,
                label: None,
            }),
        )
        .await
        .expect("the commit");

        assert_eq!(listed(&server).await, vec!["newer".to_string()]);
        assert_eq!(
            server.scalar::<i64>("SELECT COUNT(*) FROM emulation_saves").await,
            1,
            "the kept save went with the one it was kept beside"
        );
    }
}
