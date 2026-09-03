//! Cloud Save V2 — the snapshot-based save sync the launcher uses from 4.1.0.
//!
//! Where the legacy `artifacts` API stores one opaque tarball per backup, V2
//! models a save as a manifest of individual files, each content-addressed by
//! SHA-256. That buys three things the launcher depends on:
//!
//! * **Delta uploads.** `prepare-snapshot` answers with `skip` for every blob
//!   the server already holds, so only changed files cross the wire.
//! * **Conflict detection.** Each commit bumps `version`; the launcher sends
//!   the version it started from as `baseVersion`, so a second machine that
//!   uploads from stale state is rejected instead of clobbering the newer save.
//! * **Selective restore.** The restore manifest lists files individually, so
//!   the launcher can pull just the ones that differ locally.
//!
//! The launcher validates these responses strictly — exact key sets, exact
//! field counts — so the response structs here are deliberately rigid. See
//! `cloud-save-contract.ts` and `upload-local-game-snapshot-helpers.ts` in the
//! launcher for the validators these must satisfy.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::storage;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Abandoned pending snapshots are swept once they are older than this. An
/// upload that dies midway would otherwise pin its blobs forever.
const PENDING_SNAPSHOT_TTL_SECONDS: i64 = 24 * 60 * 60;

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The launcher only ever syncs these two shops, and its restore-manifest
/// validator rejects anything else outright.
fn valid_shop(shop: &str) -> bool {
    matches!(shop, "steam" | "launchbox")
}

/// A version a commit replaced and the server kept. No sync reads one, which
/// is why the by-id routes act on these and only these.
fn is_retained(status: &str) -> bool {
    status == "superseded"
}

/// `x-amz-checksum-sha256` carries the digest base64-encoded, not hex. The
/// launcher recomputes this from its own hash and refuses the response if it
/// disagrees, so the encoding has to match exactly.
fn checksum_header(hash: &str) -> ApiResult<String> {
    let raw = hex::decode(hash)
        .map_err(|_| ApiError::bad_request("invalid file hash"))?;
    Ok(BASE64.encode(raw))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFileInput {
    pub variant_id: String,
    pub raw_path: String,
    pub relative_path: String,
    pub hash: String,
    pub size_bytes: i64,
    pub last_modified_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareSnapshotRequest {
    pub shop: String,
    pub object_id: String,
    pub platform: Option<String>,
    pub hostname: Option<String>,
    pub snapshot_hash: String,
    pub base_version: i64,
    #[serde(default)]
    pub custom_path_raw_paths: Vec<String>,
    #[serde(default)]
    pub variants: Vec<Value>,
    pub files: Vec<SnapshotFileInput>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequiredHeaders {
    #[serde(rename = "Content-Length")]
    pub content_length: String,
    #[serde(rename = "x-amz-checksum-sha256")]
    pub checksum_sha256: String,
}

/// One entry per file the launcher proposed. `skip` and `upload` carry
/// different key sets and the launcher rejects any extra key, hence the
/// untagged enum rather than an optional-field struct.
#[derive(Serialize)]
#[serde(untagged)]
pub enum PrepareSnapshotFile {
    Skip {
        #[serde(rename = "variantId")]
        variant_id: String,
        #[serde(rename = "rawPath")]
        raw_path: String,
        #[serde(rename = "relativePath")]
        relative_path: String,
        status: &'static str,
    },
    Upload {
        #[serde(rename = "variantId")]
        variant_id: String,
        #[serde(rename = "rawPath")]
        raw_path: String,
        #[serde(rename = "relativePath")]
        relative_path: String,
        status: &'static str,
        #[serde(rename = "uploadUrl")]
        upload_url: String,
        #[serde(rename = "requiredHeaders")]
        required_headers: RequiredHeaders,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareSnapshotResponse {
    pub pending_snapshot_id: String,
    pub snapshot_hash: String,
    pub files: Vec<PrepareSnapshotFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSnapshotRequest {
    pub pending_snapshot_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSnapshotResponse {
    pub snapshot_id: String,
    pub version: i64,
    pub file_count: i64,
    pub total_size_bytes: i64,
    pub aggregate_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSnapshotSummary {
    pub id: String,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub file_count: i64,
    pub total_size_bytes: i64,
    pub aggregate_hash: String,
}

/// A snapshot for the launcher's Cloud Save Manager. Separate from
/// `RemoteSnapshotSummary`, which the sync path validates key by key.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySnapshotSummary {
    pub id: String,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub file_count: i64,
    pub total_size_bytes: i64,
    pub aggregate_hash: String,
    pub shop: String,
    pub object_id: String,
    pub hostname: Option<String>,
    pub platform: Option<String>,
    pub game_name: Option<String>,
    pub game_cover_url: Option<String>,
    /// `current` for the save being synced, `retained` for a kept older one.
    pub status: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestSnapshot {
    pub id: String,
    pub version: i64,
    pub shop: String,
    pub object_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub variant_id: String,
    pub raw_path: String,
    pub relative_path: String,
    pub hash: String,
    pub size_bytes: i64,
    pub last_modified_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreManifestResponse {
    pub snapshot: ManifestSnapshot,
    pub custom_path_raw_paths: Vec<String>,
    pub variants: Vec<Value>,
    pub files: Vec<ManifestFile>,
}

/// Exactly the manifest file plus a download URL — the launcher asserts the
/// object has precisely seven keys.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadUrlFile {
    pub variant_id: String,
    pub raw_path: String,
    pub relative_path: String,
    pub hash: String,
    pub size_bytes: i64,
    pub last_modified_at: String,
    pub download_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameQuery {
    pub shop: String,
    pub object_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotQuery {
    pub snapshot_id: String,
}

// ---------------------------------------------------------------------------
// POST /profile/cloud-saves/prepare-snapshot
// ---------------------------------------------------------------------------

/// Registers the manifest the launcher wants to store and hands back a
/// presigned PUT for every blob this server does not already have.
///
/// Nothing here is durable yet: the snapshot row is `pending` until
/// `commit-snapshot` verifies the bytes actually landed.
pub async fn prepare_snapshot(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<PrepareSnapshotRequest>,
) -> ApiResult<Json<PrepareSnapshotResponse>> {
    let user_id = &user.0.id;

    if !valid_shop(&payload.shop) {
        return Err(ApiError::bad_request("unsupported shop"));
    }
    if payload.object_id.is_empty() {
        return Err(ApiError::bad_request("missing objectId"));
    }
    if !is_sha256(&payload.snapshot_hash) {
        return Err(ApiError::bad_request("invalid snapshotHash"));
    }
    if payload.base_version < 0 {
        return Err(ApiError::bad_request("invalid baseVersion"));
    }

    let mut seen = HashSet::new();
    for file in &payload.files {
        if !is_sha256(&file.hash) || !is_sha256(&file.variant_id) {
            return Err(ApiError::bad_request("invalid file hash"));
        }
        if file.size_bytes < 0 {
            return Err(ApiError::bad_request("invalid file size"));
        }
        if file.raw_path.is_empty() || file.relative_path.is_empty() {
            return Err(ApiError::bad_request("invalid file path"));
        }
        if !seen.insert((
            file.variant_id.as_str(),
            file.raw_path.as_str(),
            file.relative_path.as_str(),
        )) {
            return Err(ApiError::bad_request("duplicate file identity"));
        }
    }

    sweep_stale_pending(&state, user_id).await?;

    /* Optimistic concurrency. `baseVersion` is the version the launcher
       started from; if the stored snapshot has moved on, another machine
       committed in the meantime and this upload would lose that work. */
    let current: Option<(String, i64)> = sqlx::query_as(
        "SELECT id, version FROM cloud_save_snapshots
         WHERE user_id = ? AND shop = ? AND object_id = ? AND status = 'committed'",
    )
    .bind(user_id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .fetch_optional(&state.pool)
    .await?;

    let current_version = current.as_ref().map(|(_, version)| *version).unwrap_or(0);
    if payload.base_version != current_version {
        /* Worth logging: a conflict is the visible half of "my save went
           backwards on the other machine", and the panel can show it. */
        crate::events::record(
            &state,
            Event::sync(
                "cloud_save.conflict",
                user_id,
                "Upload refused — the cloud save had already moved on",
            )
            .game(&payload.shop, &payload.object_id)
            .detail(serde_json::json!({
                "baseVersion": payload.base_version,
                "currentVersion": current_version,
                "hostname": payload.hostname,
            }))
            .warning(),
        )
        .await;

        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "cloud save has changed on another device — sync again before uploading",
        ));
    }

    /* Which blobs do we already hold? Deduplicate by hash first: the same
       bytes can appear under several identities, and the launcher uploads
       each distinct hash only once. */
    let mut sizes_by_hash: HashMap<&str, i64> = HashMap::new();
    for file in &payload.files {
        sizes_by_hash.insert(&file.hash, file.size_bytes);
    }

    let mut existing: HashSet<String> = HashSet::new();
    for hash in sizes_by_hash.keys() {
        let present: Option<(i64,)> = sqlx::query_as(
            "SELECT size_in_bytes FROM cloud_save_blobs WHERE user_id = ? AND hash = ?",
        )
        .bind(user_id)
        .bind(hash)
        .fetch_optional(&state.pool)
        .await?;

        /* Trust the row only if the bytes are really still on disk — a
           half-cleaned storage dir must not turn into a silent data loss. */
        if present.is_some() {
            let path = storage::storage_path(
                &state,
                &storage::cloud_save_blob_key(user_id, hash),
            );
            if tokio::fs::metadata(&path).await.is_ok() {
                existing.insert((*hash).to_string());
            }
        }
    }

    let incoming_bytes: i64 = sizes_by_hash
        .iter()
        .filter(|(hash, _)| !existing.contains(**hash))
        .map(|(_, size)| *size)
        .sum();

    enforce_quota(&state, user_id, incoming_bytes).await?;

    let now = Utc::now().to_rfc3339();
    let snapshot_id = Uuid::new_v4().to_string();
    let total_size: i64 = payload.files.iter().map(|file| file.size_bytes).sum();

    let custom_paths = serde_json::to_string(&payload.custom_path_raw_paths)
        .unwrap_or_else(|_| "[]".to_string());
    let variants =
        serde_json::to_string(&payload.variants).unwrap_or_else(|_| "[]".to_string());

    let mut tx = state.pool.begin().await?;

    sqlx::query(
        "INSERT INTO cloud_save_snapshots
           (id, user_id, shop, object_id, version, aggregate_hash, file_count,
            total_size_in_bytes, platform, hostname, custom_path_raw_paths,
            variants, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(&snapshot_id)
    .bind(user_id)
    .bind(&payload.shop)
    .bind(&payload.object_id)
    .bind(current_version + 1)
    .bind(&payload.snapshot_hash)
    .bind(payload.files.len() as i64)
    .bind(total_size)
    .bind(&payload.platform)
    .bind(&payload.hostname)
    .bind(&custom_paths)
    .bind(&variants)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    for file in &payload.files {
        sqlx::query(
            "INSERT INTO cloud_save_snapshot_files
               (snapshot_id, variant_id, raw_path, relative_path, hash,
                size_in_bytes, last_modified_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&snapshot_id)
        .bind(&file.variant_id)
        .bind(&file.raw_path)
        .bind(&file.relative_path)
        .bind(&file.hash)
        .bind(file.size_bytes)
        .bind(&file.last_modified_at)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    let mut files = Vec::with_capacity(payload.files.len());
    for file in &payload.files {
        if existing.contains(&file.hash) {
            files.push(PrepareSnapshotFile::Skip {
                variant_id: file.variant_id.clone(),
                raw_path: file.raw_path.clone(),
                relative_path: file.relative_path.clone(),
                status: "skip",
            });
        } else {
            /* Every identity sharing a hash gets the same content-addressed
               URL, so the launcher uploading one of them satisfies them all. */
            files.push(PrepareSnapshotFile::Upload {
                variant_id: file.variant_id.clone(),
                raw_path: file.raw_path.clone(),
                relative_path: file.relative_path.clone(),
                status: "upload",
                upload_url: storage::sign_blob_upload_url(
                    &state,
                    user_id,
                    &file.hash,
                    file.size_bytes as u64,
                ),
                required_headers: RequiredHeaders {
                    content_length: file.size_bytes.to_string(),
                    checksum_sha256: checksum_header(&file.hash)?,
                },
            });
        }
    }

    Ok(Json(PrepareSnapshotResponse {
        pending_snapshot_id: snapshot_id,
        snapshot_hash: payload.snapshot_hash,
        files,
    }))
}

// ---------------------------------------------------------------------------
// POST /profile/cloud-saves/commit-snapshot
// ---------------------------------------------------------------------------

/// Promotes a pending snapshot to the game's current save.
///
/// Every referenced blob must be on disk with the right size before the
/// snapshot becomes visible, so a partially failed upload can never be handed
/// back to the launcher as a restorable save.
pub async fn commit_snapshot(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(payload): Json<CommitSnapshotRequest>,
) -> ApiResult<Json<CommitSnapshotResponse>> {
    let user_id = &user.0.id;

    let snapshot = sqlx::query(
        "SELECT * FROM cloud_save_snapshots
         WHERE id = ? AND user_id = ? AND status = 'pending'",
    )
    .bind(&payload.pending_snapshot_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("pending snapshot not found"))?;

    let shop: String = snapshot.get("shop");
    let object_id: String = snapshot.get("object_id");
    let version: i64 = snapshot.get("version");
    let aggregate_hash: String = snapshot.get("aggregate_hash");

    let rows = sqlx::query(
        "SELECT DISTINCT hash, size_in_bytes FROM cloud_save_snapshot_files
         WHERE snapshot_id = ?",
    )
    .bind(&payload.pending_snapshot_id)
    .fetch_all(&state.pool)
    .await?;

    let now = Utc::now().to_rfc3339();

    for row in &rows {
        let hash: String = row.get("hash");
        let expected: i64 = row.get("size_in_bytes");
        let key = storage::cloud_save_blob_key(user_id, &hash);
        let path = storage::storage_path(&state, &key);

        let metadata = tokio::fs::metadata(&path).await.map_err(|_| {
            ApiError::bad_request("a snapshot file was never uploaded")
        })?;

        if metadata.len() as i64 != expected {
            return Err(ApiError::bad_request(
                "an uploaded snapshot file has an unexpected size",
            ));
        }

        /* The bytes were hash-verified during upload, so registering the blob
           here is safe. Existing rows keep their original created_at. */
        sqlx::query(
            "INSERT INTO cloud_save_blobs (user_id, hash, size_in_bytes, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(user_id, hash) DO UPDATE SET size_in_bytes = excluded.size_in_bytes",
        )
        .bind(user_id)
        .bind(&hash)
        .bind(expected)
        .bind(&now)
        .execute(&state.pool)
        .await?;
    }

    let auto_delete = crate::limits::for_user(&state, user_id)
        .await?
        .auto_delete_saves;

    let mut tx = state.pool.begin().await?;

    let superseded: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM cloud_save_snapshots
         WHERE user_id = ? AND shop = ? AND object_id = ? AND status = 'committed'",
    )
    .bind(user_id)
    .bind(&shop)
    .bind(&object_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut retained = 0usize;

    if auto_delete {
        let kept: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM cloud_save_snapshots
             WHERE user_id = ? AND shop = ? AND object_id = ? AND status = 'superseded'",
        )
        .bind(user_id)
        .bind(&shop)
        .bind(&object_id)
        .fetch_all(&mut *tx)
        .await?;

        for id in superseded.iter().chain(kept.iter()) {
            sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    } else {
        for id in &superseded {
            sqlx::query(
                "UPDATE cloud_save_snapshots SET status = 'superseded', updated_at = ?
                 WHERE id = ?",
            )
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        retained = superseded.len();
    }

    sqlx::query(
        "UPDATE cloud_save_snapshots SET status = 'committed', updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(&payload.pending_snapshot_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    collect_orphan_blobs(&state, user_id).await?;

    let file_count: i64 = snapshot.get("file_count");
    let total_size: i64 = snapshot.get("total_size_in_bytes");

    tracing::info!(
        "cloud save v2: committed {shop}:{object_id} v{version} ({file_count} files) for {user_id}"
    );

    crate::events::record(
        &state,
        Event::sync(
            "cloud_save.committed",
            user_id,
            format!("Synced a cloud save (v{version}, {file_count} files)"),
        )
        .game(&shop, &object_id)
        .detail(serde_json::json!({
            "snapshotId": payload.pending_snapshot_id,
            "version": version,
            "fileCount": file_count,
            "hostname": snapshot.get::<Option<String>, _>("hostname"),
            "platform": snapshot.get::<Option<String>, _>("platform"),
            "retainedVersions": retained,
        }))
        .size(total_size),
    )
    .await;

    Ok(Json(CommitSnapshotResponse {
        snapshot_id: payload.pending_snapshot_id,
        version,
        file_count,
        total_size_bytes: total_size,
        aggregate_hash,
    }))
}

// ---------------------------------------------------------------------------
// GET /profile/cloud-saves/all-snapshots
// ---------------------------------------------------------------------------

/// Every stored snapshot of this user, current and retained, so the manager
/// needs one request instead of one per game. Pending uploads are left out.
pub async fn list_all_snapshots(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<LibrarySnapshotSummary>>> {
    let rows = sqlx::query(
        "SELECT s.*, g.name AS game_name, g.cover_url AS game_cover_url
         FROM cloud_save_snapshots s
         LEFT JOIN game_metadata g ON g.shop = s.shop AND g.object_id = s.object_id
         WHERE s.user_id = ? AND s.status IN ('committed', 'superseded')
         ORDER BY s.updated_at DESC",
    )
    .bind(&user.0.id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|row| LibrarySnapshotSummary {
                id: row.get("id"),
                version: row.get("version"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
                file_count: row.get("file_count"),
                total_size_bytes: row.get("total_size_in_bytes"),
                aggregate_hash: row.get("aggregate_hash"),
                shop: row.get("shop"),
                object_id: row.get("object_id"),
                hostname: row.get("hostname"),
                platform: row.get("platform"),
                game_name: row.try_get("game_name").unwrap_or(None),
                game_cover_url: row.try_get("game_cover_url").unwrap_or(None),
                status: if row.get::<String, _>("status") == "committed" {
                    "current"
                } else {
                    "retained"
                },
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// GET / DELETE /profile/cloud-saves/snapshots
// ---------------------------------------------------------------------------

/// Lists the committed snapshot for a game. At most one exists, but the
/// launcher expects an array.
pub async fn list_snapshots(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<GameQuery>,
) -> ApiResult<Json<Vec<RemoteSnapshotSummary>>> {
    if !valid_shop(&query.shop) {
        return Err(ApiError::bad_request("unsupported shop"));
    }

    let rows = sqlx::query(
        "SELECT * FROM cloud_save_snapshots
         WHERE user_id = ? AND shop = ? AND object_id = ? AND status = 'committed'
         ORDER BY version DESC",
    )
    .bind(&user.0.id)
    .bind(&query.shop)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.iter()
            .map(|row| RemoteSnapshotSummary {
                id: row.get("id"),
                version: row.get("version"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
                file_count: row.get("file_count"),
                total_size_bytes: row.get("total_size_in_bytes"),
                aggregate_hash: row.get("aggregate_hash"),
            })
            .collect(),
    ))
}

/// Removes every snapshot for a game and frees any blob it alone referenced.
pub async fn delete_snapshots(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<GameQuery>,
) -> ApiResult<StatusCode> {
    if !valid_shop(&query.shop) {
        return Err(ApiError::bad_request("unsupported shop"));
    }

    let user_id = &user.0.id;

    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM cloud_save_snapshots
         WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(&query.shop)
    .bind(&query.object_id)
    .fetch_all(&state.pool)
    .await?;

    for id in &ids {
        sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    collect_orphan_blobs(&state, user_id).await?;

    tracing::info!(
        "cloud save v2: deleted {}:{} for {user_id}",
        query.shop,
        query.object_id
    );

    crate::events::record(
        &state,
        Event::sync("cloud_save.deleted", user_id, "Deleted a cloud save from the launcher")
            .game(&query.shop, &query.object_id)
            .detail(serde_json::json!({ "snapshots": ids.len() })),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// DELETE /profile/cloud-saves/snapshots/{id}
// ---------------------------------------------------------------------------

/// Removes one retained version and frees any blob it alone referenced.
pub async fn delete_snapshot(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let user_id = &user.0.id;

    let snapshot = sqlx::query(
        "SELECT shop, object_id, status, total_size_in_bytes
         FROM cloud_save_snapshots WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("snapshot not found"))?;

    /* Never the save in use: launchers hold a sync anchor for it, and would
       read its files as remotely deleted. That goes through the per-game
       delete, which clears the launcher's local state too. */
    if !is_retained(&snapshot.get::<String, _>("status")) {
        return Err(ApiError::bad_request(
            "only a retained version can be deleted on its own",
        ));
    }

    let shop: String = snapshot.get("shop");
    let object_id: String = snapshot.get("object_id");

    sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    collect_orphan_blobs(&state, user_id).await?;

    tracing::info!("cloud save v2: deleted retained version {id} for {user_id}");

    crate::events::record(
        &state,
        Event::sync(
            "cloud_save.deleted",
            user_id,
            "Deleted a kept cloud save version from the launcher",
        )
        .game(&shop, &object_id)
        .detail(serde_json::json!({ "snapshotId": id, "retained": true }))
        .size(snapshot.get::<i64, _>("total_size_in_bytes")),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// POST /profile/cloud-saves/snapshots/{id}/restore
// ---------------------------------------------------------------------------

/// Puts a retained version back in use as the game's current save.
///
/// The two versions swap places — no bytes move, so it is reversible — and
/// every launcher restores it on its next sync like any other remote change.
pub async fn restore_snapshot(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<CommitSnapshotResponse>> {
    let user_id = &user.0.id;

    let snapshot = sqlx::query(
        "SELECT shop, object_id, status, file_count, total_size_in_bytes, aggregate_hash
         FROM cloud_save_snapshots WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("snapshot not found"))?;

    if !is_retained(&snapshot.get::<String, _>("status")) {
        return Err(ApiError::bad_request(
            "only a retained version can be restored",
        ));
    }

    let shop: String = snapshot.get("shop");
    let object_id: String = snapshot.get("object_id");
    let now = Utc::now().to_rfc3339();

    let mut tx = state.pool.begin().await?;

    /* Past every version this game has held, so launchers see it as newer. */
    let next_version: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM cloud_save_snapshots
         WHERE user_id = ? AND shop = ? AND object_id = ?",
    )
    .bind(user_id)
    .bind(&shop)
    .bind(&object_id)
    .fetch_one(&mut *tx)
    .await?;

    /* Demote first: only one snapshot per game may be committed. The one
       stepping aside is kept, so the restore can be undone. */
    sqlx::query(
        "UPDATE cloud_save_snapshots SET status = 'superseded', updated_at = ?
         WHERE user_id = ? AND shop = ? AND object_id = ? AND status = 'committed'",
    )
    .bind(&now)
    .bind(user_id)
    .bind(&shop)
    .bind(&object_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE cloud_save_snapshots SET status = 'committed', version = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(next_version)
    .bind(&now)
    .bind(&id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let file_count: i64 = snapshot.get("file_count");
    let total_size: i64 = snapshot.get("total_size_in_bytes");
    let aggregate_hash: String = snapshot.get("aggregate_hash");

    tracing::info!(
        "cloud save v2: restored {shop}:{object_id} to v{next_version} ({file_count} files) for {user_id}"
    );

    crate::events::record(
        &state,
        Event::sync(
            "cloud_save.restored",
            user_id,
            format!("Restored a kept cloud save version (now v{next_version}, {file_count} files)"),
        )
        .game(&shop, &object_id)
        .detail(serde_json::json!({
            "snapshotId": id,
            "version": next_version,
            "fileCount": file_count,
        }))
        .size(total_size),
    )
    .await;

    Ok(Json(CommitSnapshotResponse {
        snapshot_id: id,
        version: next_version,
        file_count,
        total_size_bytes: total_size,
        aggregate_hash,
    }))
}

// ---------------------------------------------------------------------------
// GET /profile/cloud-saves/snapshot-restore-manifest
// ---------------------------------------------------------------------------

/// The full manifest for a snapshot, which the launcher diffs against local
/// state to decide what to restore.
pub async fn restore_manifest(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<SnapshotQuery>,
) -> ApiResult<Json<RestoreManifestResponse>> {
    let snapshot = fetch_committed(&state, &user.0.id, &query.snapshot_id).await?;

    let files = fetch_manifest_files(&state, &query.snapshot_id).await?;

    let custom_path_raw_paths: Vec<String> =
        serde_json::from_str(&snapshot.get::<String, _>("custom_path_raw_paths"))
            .unwrap_or_default();
    let variants: Vec<Value> =
        serde_json::from_str(&snapshot.get::<String, _>("variants")).unwrap_or_default();

    Ok(Json(RestoreManifestResponse {
        snapshot: ManifestSnapshot {
            id: snapshot.get("id"),
            version: snapshot.get("version"),
            shop: snapshot.get("shop"),
            object_id: snapshot.get("object_id"),
        },
        custom_path_raw_paths,
        variants,
        files,
    }))
}

// ---------------------------------------------------------------------------
// GET /profile/cloud-saves/snapshot-download-urls
// ---------------------------------------------------------------------------

/// Presigned GETs for every file in a snapshot.
pub async fn snapshot_download_urls(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<SnapshotQuery>,
) -> ApiResult<Json<Vec<DownloadUrlFile>>> {
    let user_id = &user.0.id;
    fetch_committed(&state, user_id, &query.snapshot_id).await?;

    let files = fetch_manifest_files(&state, &query.snapshot_id).await?;

    Ok(Json(
        files
            .into_iter()
            .map(|file| {
                let url = storage::sign_download_url(
                    &state,
                    &storage::cloud_save_blob_key(user_id, &file.hash),
                );
                DownloadUrlFile {
                    variant_id: file.variant_id,
                    raw_path: file.raw_path,
                    relative_path: file.relative_path,
                    hash: file.hash,
                    size_bytes: file.size_bytes,
                    last_modified_at: file.last_modified_at,
                    download_url: url,
                }
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn fetch_committed(
    state: &AppState,
    user_id: &str,
    snapshot_id: &str,
) -> ApiResult<sqlx::sqlite::SqliteRow> {
    sqlx::query(
        "SELECT * FROM cloud_save_snapshots
         WHERE id = ? AND user_id = ? AND status = 'committed'",
    )
    .bind(snapshot_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("snapshot not found"))
}

async fn fetch_manifest_files(
    state: &AppState,
    snapshot_id: &str,
) -> ApiResult<Vec<ManifestFile>> {
    let rows = sqlx::query(
        "SELECT variant_id, raw_path, relative_path, hash, size_in_bytes, last_modified_at
         FROM cloud_save_snapshot_files WHERE snapshot_id = ?",
    )
    .bind(snapshot_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| ManifestFile {
            variant_id: row.get("variant_id"),
            raw_path: row.get("raw_path"),
            relative_path: row.get("relative_path"),
            hash: row.get("hash"),
            size_bytes: row.get("size_in_bytes"),
            last_modified_at: row.get("last_modified_at"),
        })
        .collect())
}

/// Counts only the blobs this upload would actually add — files the server
/// already holds cost nothing, so re-syncing an unchanged save never trips the
/// quota.
async fn enforce_quota(
    state: &AppState,
    user_id: &str,
    incoming_bytes: i64,
) -> ApiResult<()> {
    let max_bytes_per_user = crate::limits::for_user(state, user_id)
        .await?
        .max_bytes_per_user;
    if max_bytes_per_user == 0 {
        return Ok(());
    }

    let used = storage::used_bytes(state, user_id).await?;
    if storage::exceeds_quota(max_bytes_per_user, used, incoming_bytes) {
        crate::events::record(
            state,
            Event::sync("cloud_save.quota_exceeded", user_id, "Upload refused — quota full")
                .detail(serde_json::json!({
                    "usedBytes": used,
                    "incomingBytes": incoming_bytes,
                    "quotaBytes": max_bytes_per_user,
                }))
                .warning(),
        )
        .await;

        return Err(storage::quota_error());
    }

    Ok(())
}

/// Drops pending snapshots that were never committed, so an interrupted
/// upload does not keep its blobs alive forever.
async fn sweep_stale_pending(state: &AppState, user_id: &str) -> ApiResult<()> {
    let cutoff = (Utc::now() - chrono::Duration::seconds(PENDING_SNAPSHOT_TTL_SECONDS))
        .to_rfc3339();

    let stale: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM cloud_save_snapshots
         WHERE user_id = ? AND status = 'pending' AND created_at < ?",
    )
    .bind(user_id)
    .bind(&cutoff)
    .fetch_all(&state.pool)
    .await?;

    for id in &stale {
        sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    if !stale.is_empty() {
        collect_orphan_blobs(state, user_id).await?;
    }

    Ok(())
}

/// Deletes blobs no manifest row references any more, on disk and in the
/// blob table, so the quota reflects only live data.
///
/// The admin panel calls this too, after deleting a snapshot on a user's
/// behalf.
pub async fn collect_orphan_blobs(state: &AppState, user_id: &str) -> ApiResult<()> {
    let orphans: Vec<String> = sqlx::query_scalar(
        "SELECT b.hash FROM cloud_save_blobs b
         WHERE b.user_id = ?
           AND NOT EXISTS (
             SELECT 1 FROM cloud_save_snapshot_files f
             JOIN cloud_save_snapshots s ON s.id = f.snapshot_id
             WHERE s.user_id = b.user_id AND f.hash = b.hash
           )",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;

    for hash in &orphans {
        storage::delete_object(state, &storage::cloud_save_blob_key(user_id, hash)).await;
        sqlx::query("DELETE FROM cloud_save_blobs WHERE user_id = ? AND hash = ?")
            .bind(user_id)
            .bind(hash)
            .execute(&state.pool)
            .await?;
    }

    if !orphans.is_empty() {
        tracing::info!(
            "cloud save v2: freed {} orphaned blob(s) for {user_id}",
            orphans.len()
        );
        crate::events::record(
            state,
            Event::system(
                "system.gc",
                format!("Freed {} orphaned blob(s)", orphans.len()),
            )
            .about(user_id)
            .detail(serde_json::json!({ "blobs": orphans.len() })),
        )
        .await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The launcher rejects a response that carries even one unexpected key, so
/// the wire shapes are pinned here rather than discovered in production.
/// See `upload-local-game-snapshot-helpers.ts` for the validator these mirror.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("expected an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn skip_files_carry_exactly_four_keys() {
        let file = PrepareSnapshotFile::Skip {
            variant_id: "a".repeat(64),
            raw_path: "<gameDir>".into(),
            relative_path: "slot1.sav".into(),
            status: "skip",
        };
        let value = serde_json::to_value(&file).unwrap();

        assert_eq!(
            keys(&value),
            vec!["rawPath", "relativePath", "status", "variantId"]
        );
        assert_eq!(value["status"], "skip");
    }

    #[test]
    fn upload_files_carry_exactly_six_keys_and_two_headers() {
        let file = PrepareSnapshotFile::Upload {
            variant_id: "a".repeat(64),
            raw_path: "<gameDir>".into(),
            relative_path: "slot1.sav".into(),
            status: "upload",
            upload_url: "http://example.test/storage/token".into(),
            required_headers: RequiredHeaders {
                content_length: "14".into(),
                checksum_sha256: "abc".into(),
            },
        };
        let value = serde_json::to_value(&file).unwrap();

        assert_eq!(
            keys(&value),
            vec![
                "rawPath",
                "relativePath",
                "requiredHeaders",
                "status",
                "uploadUrl",
                "variantId"
            ]
        );
        assert_eq!(
            keys(&value["requiredHeaders"]),
            vec!["Content-Length", "x-amz-checksum-sha256"]
        );
    }

    #[test]
    fn snapshot_summary_matches_the_launcher_shape() {
        let value = serde_json::to_value(RemoteSnapshotSummary {
            id: "id".into(),
            version: 1,
            created_at: "2026-08-01T10:00:00Z".into(),
            updated_at: "2026-08-01T10:00:00Z".into(),
            file_count: 3,
            total_size_bytes: 42,
            aggregate_hash: "b".repeat(64),
        })
        .unwrap();

        assert_eq!(
            keys(&value),
            vec![
                "aggregateHash",
                "createdAt",
                "fileCount",
                "id",
                "totalSizeBytes",
                "updatedAt",
                "version"
            ]
        );
    }

    #[test]
    fn only_a_retained_version_answers_to_the_by_id_routes() {
        assert!(is_retained("superseded"));
        assert!(!is_retained("committed"));
        assert!(!is_retained("pending"));
    }

    #[test]
    fn library_snapshot_summary_extends_the_launcher_shape() {
        let value = serde_json::to_value(LibrarySnapshotSummary {
            id: "id".into(),
            version: 1,
            created_at: "2026-08-01T10:00:00Z".into(),
            updated_at: "2026-08-01T10:00:00Z".into(),
            file_count: 3,
            total_size_bytes: 42,
            aggregate_hash: "b".repeat(64),
            shop: "steam".into(),
            object_id: "440".into(),
            hostname: Some("desktop".into()),
            platform: Some("windows".into()),
            game_name: None,
            game_cover_url: None,
            status: "current",
        })
        .unwrap();

        assert_eq!(
            keys(&value),
            vec![
                "aggregateHash",
                "createdAt",
                "fileCount",
                "gameCoverUrl",
                "gameName",
                "hostname",
                "id",
                "objectId",
                "platform",
                "shop",
                "status",
                "totalSizeBytes",
                "updatedAt",
                "version"
            ]
        );
    }

    #[test]
    fn download_url_entries_carry_exactly_seven_keys() {
        let value = serde_json::to_value(DownloadUrlFile {
            variant_id: "a".repeat(64),
            raw_path: "<gameDir>".into(),
            relative_path: "slot1.sav".into(),
            hash: "b".repeat(64),
            size_bytes: 14,
            last_modified_at: "2026-08-01T10:00:00Z".into(),
            download_url: "http://example.test/storage/token".into(),
        })
        .unwrap();

        assert_eq!(value.as_object().unwrap().len(), 7);
        assert_eq!(
            keys(&value),
            vec![
                "downloadUrl",
                "hash",
                "lastModifiedAt",
                "rawPath",
                "relativePath",
                "sizeBytes",
                "variantId"
            ]
        );
    }

    #[test]
    fn restore_manifest_snapshot_carries_exactly_four_keys() {
        let value = serde_json::to_value(RestoreManifestResponse {
            snapshot: ManifestSnapshot {
                id: "id".into(),
                version: 2,
                shop: "steam".into(),
                object_id: "440".into(),
            },
            custom_path_raw_paths: vec![],
            variants: vec![json!({ "variantId": "a".repeat(64), "kind": "default" })],
            files: vec![],
        })
        .unwrap();

        assert_eq!(
            keys(&value),
            vec!["customPathRawPaths", "files", "snapshot", "variants"]
        );
        assert_eq!(
            keys(&value["snapshot"]),
            vec!["id", "objectId", "shop", "version"]
        );
    }

    /// The launcher recomputes this from its own hash and refuses the response
    /// if it disagrees, so hex-in / base64-out has to be exact.
    #[test]
    fn checksum_header_is_base64_of_the_raw_digest() {
        // SHA-256 of the empty string.
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            checksum_header(hash).unwrap(),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
        assert!(checksum_header("not-hex").is_err());
    }

    #[test]
    fn only_steam_and_launchbox_are_accepted() {
        assert!(valid_shop("steam"));
        assert!(valid_shop("launchbox"));
        assert!(!valid_shop("custom"));
        assert!(!valid_shop(""));
    }

    #[test]
    fn sha256_guard_rejects_non_digests() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"a".repeat(63)));
        assert!(!is_sha256(&"g".repeat(64)));
    }

    use crate::limits::Overrides;
    use crate::testing::TestServer;

    /// A game with a committed snapshot holding one blob, and a pending one
    /// holding a different blob — so what survives the commit says exactly
    /// which version's bytes were kept.
    async fn a_game_mid_sync(server: &TestServer) {
        for (id, hash, size, status, version) in [
            ("old", &"a".repeat(64), 100, "committed", 1),
            ("new", &"b".repeat(64), 200, "pending", 2),
        ] {
            sqlx::query(
                "INSERT INTO cloud_save_snapshots
                   (id, user_id, shop, object_id, version, aggregate_hash,
                    file_count, total_size_in_bytes, status, created_at, updated_at)
                 VALUES (?, 'alice', 'steam', '440', ?, ?, 1, ?, ?,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(version)
            .bind(hash)
            .bind(size)
            .bind(status)
            .execute(&server.state.pool)
            .await
            .expect("a snapshot");

            sqlx::query(
                "INSERT INTO cloud_save_snapshot_files
                   (snapshot_id, variant_id, raw_path, relative_path, hash,
                    size_in_bytes, last_modified_at)
                 VALUES (?, 'default', '<winPrefix>', 'save.dat', ?, ?,
                         '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(hash)
            .bind(size)
            .execute(&server.state.pool)
            .await
            .expect("a manifest row");

            let key = storage::cloud_save_blob_key("alice", hash);
            let path = storage::storage_path(&server.state, &key);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the blob dir");
            std::fs::write(&path, vec![7u8; size as usize]).expect("the blob");
        }

        sqlx::query(
            "INSERT INTO cloud_save_blobs (user_id, hash, size_in_bytes, created_at)
             VALUES ('alice', ?, 100, '2026-01-01T00:00:00Z')",
        )
        .bind("a".repeat(64))
        .execute(&server.state.pool)
        .await
        .expect("the committed blob");
    }

    async fn commit_the_pending_snapshot(server: &TestServer) {
        let _ = commit_snapshot(
            State(server.state.clone()),
            server.user("alice"),
            Json(CommitSnapshotRequest {
                pending_snapshot_id: "new".to_string(),
            }),
        )
        .await
        .expect("the commit");
    }

    async fn status_of(server: &TestServer, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM cloud_save_snapshots WHERE id = ?")
            .bind(id)
            .fetch_optional(&server.state.pool)
            .await
            .expect("the snapshot")
    }

    #[tokio::test]
    async fn the_replaced_version_is_deleted_by_default() {
        let server = TestServer::start().await;
        a_game_mid_sync(&server).await;

        commit_the_pending_snapshot(&server).await;

        assert_eq!(status_of(&server, "new").await.as_deref(), Some("committed"));
        assert_eq!(status_of(&server, "old").await, None);
        assert_eq!(storage::used_bytes(&server.state, "alice").await.unwrap(), 200);
        assert!(!storage::storage_path(
            &server.state,
            &storage::cloud_save_blob_key("alice", &"a".repeat(64))
        )
        .exists());
    }

    /// Off, the replaced version is kept as 'superseded': the launcher still
    /// sees exactly one committed snapshot, and the older one's blobs stay —
    /// on the owner's quota until someone deletes them.
    #[tokio::test]
    async fn the_replaced_version_is_kept_when_the_user_opts_out() {
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
        a_game_mid_sync(&server).await;

        commit_the_pending_snapshot(&server).await;

        assert_eq!(status_of(&server, "new").await.as_deref(), Some("committed"));
        assert_eq!(status_of(&server, "old").await.as_deref(), Some("superseded"));
        assert_eq!(storage::used_bytes(&server.state, "alice").await.unwrap(), 300);

        let Json(listed) = list_snapshots(
            State(server.state.clone()),
            server.user("alice"),
            Query(GameQuery {
                shop: "steam".to_string(),
                object_id: "440".to_string(),
            }),
        )
        .await
        .expect("the listing");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "new");
    }

    /// Switching deletion back on clears the versions it kept, on the game's
    /// next sync.
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
        a_game_mid_sync(&server).await;
        commit_the_pending_snapshot(&server).await;

        server.limits("alice", Overrides::default()).await;
        server
            .execute(
                "INSERT INTO cloud_save_snapshots
                   (id, user_id, shop, object_id, version, aggregate_hash,
                    file_count, total_size_in_bytes, status, created_at, updated_at)
                 VALUES ('newer', 'alice', 'steam', '440', 3, 'ccc', 0, 0, 'pending',
                         '2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z')",
            )
            .await;

        let _ = commit_snapshot(
            State(server.state.clone()),
            server.user("alice"),
            Json(CommitSnapshotRequest {
                pending_snapshot_id: "newer".to_string(),
            }),
        )
        .await
        .expect("the commit");

        assert_eq!(
            server
                .scalar::<i64>("SELECT COUNT(*) FROM cloud_save_snapshots")
                .await,
            1,
            "the kept version went with the one it was kept beside"
        );
        assert_eq!(storage::used_bytes(&server.state, "alice").await.unwrap(), 0);
    }
}
