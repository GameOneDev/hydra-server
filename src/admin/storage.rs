//! What is actually on disk, and whether it still matches the database.
//!
//! Every other screen trusts the database. This one doesn't: it walks the
//! storage directory and reconciles both directions, because the two ways
//! this server can lose data quietly are a file disappearing under a live row
//! (a restore that comes back short) and a row disappearing over a live file
//! (bytes nobody can reach and no quota counts).

use super::AdminSession;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/storage", get(overview))
        .route("/admin/api/storage/integrity", get(integrity))
}

/// One directory of the storage tree, as the panel presents it.
struct Tree {
    files: u64,
    bytes: u64,
}

/// Adds up a directory recursively. Storage trees here are shallow (a few
/// thousand files at most on a family-sized server), so a walk on demand
/// beats keeping a second set of counters in sync with the filesystem.
async fn measure(root: &std::path::Path) -> Tree {
    let mut tree = Tree { files: 0, bytes: 0 };
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                tree.files += 1;
                tree.bytes += meta.len();
            }
        }
    }

    tree
}

/// GET /admin/api/storage — disk usage per area, measured from disk rather
/// than from the database, next to what the database believes.
async fn overview(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let root = state.config.storage_dir();

    let mut areas = Vec::new();
    let mut disk_total = 0u64;
    for (key, label, dir) in [
        ("cloudSaves", "Cloud saves (v2)", "cloud-saves"),
        ("backups", "Save backups", "artifacts"),
        ("emulationSaves", "Emulation saves", "emulation-saves"),
        ("artwork", "Custom images", "images/artwork"),
        ("souvenirs", "Achievement souvenirs", "images/souvenirs"),
        ("banners", "Profile banners", "images/banners"),
        ("avatars", "Profile avatars", "images/avatars"),
    ] {
        let tree = measure(&root.join(dir)).await;
        disk_total += tree.bytes;
        areas.push(json!({
            "key": key,
            "label": label,
            "path": dir,
            "files": tree.files,
            "bytes": tree.bytes,
        }));
    }

    let expected: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT 'cloudSaves', COUNT(*), COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs
         UNION ALL
         SELECT 'backups', COUNT(*), COALESCE(SUM(artifact_length_in_bytes), 0)
           FROM artifacts WHERE is_uploaded = 1
         UNION ALL
         SELECT 'emulationSaves', COUNT(*), COALESCE(SUM(artifact_length_in_bytes), 0)
           FROM emulation_saves WHERE is_uploaded = 1
         UNION ALL
         SELECT 'artwork', COUNT(*), COALESCE(SUM(size_in_bytes), 0)
           FROM game_artwork WHERE storage_key IS NOT NULL
         UNION ALL
         SELECT 'souvenirs', COUNT(*), COALESCE(SUM(size_in_bytes), 0)
           FROM souvenirs WHERE is_uploaded = 1",
    )
    .fetch_all(&state.pool)
    .await?;

    let expected: HashMap<String, (i64, i64)> = expected
        .into_iter()
        .map(|(key, rows, bytes)| (key, (rows, bytes)))
        .collect();

    for area in &mut areas {
        let key = area["key"].as_str().unwrap_or_default().to_string();
        let (rows, bytes) = expected.get(&key).copied().unwrap_or((0, 0));
        area["expectedRows"] = json!(rows);
        area["expectedBytes"] = json!(bytes);
        /* Profile images are the one area with no row-per-file to compare
           against — banners are a column, avatars are proxied. */
        area["tracked"] = json!(expected.contains_key(&key));
    }

    let db_path = state.config.database_path();
    let mut database_bytes = 0u64;
    let mut database_files = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            database_bytes += meta.len();
            database_files.push(json!({
                "name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
                "bytes": meta.len(),
            }));
        }
    }

    Ok(Json(json!({
        "root": root.display().to_string(),
        "areas": areas,
        "diskBytes": disk_total,
        "database": { "bytes": database_bytes, "files": database_files },
    })))
}

/// A single reconciliation finding.
fn finding(kind: &str, key: String, detail: Value) -> Value {
    json!({ "kind": kind, "key": key, "detail": detail })
}

/// GET /admin/api/storage/integrity — reconcile database against disk, both
/// directions.
///
/// Read-only by design: it reports, the maintenance screen acts. An operator
/// should always get to see what is about to be deleted.
async fn integrity(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let root = state.config.storage_dir();
    let mut missing: Vec<Value> = Vec::new();
    let mut orphans: Vec<Value> = Vec::new();
    let mut orphan_bytes: u64 = 0;

    // --- rows whose bytes are gone ------------------------------------------
    let blobs = sqlx::query("SELECT user_id, hash, size_in_bytes FROM cloud_save_blobs")
        .fetch_all(&state.pool)
        .await?;
    let mut known: std::collections::HashSet<String> = Default::default();
    for row in &blobs {
        let user_id: String = row.get("user_id");
        let hash: String = row.get("hash");
        let key = crate::storage::cloud_save_blob_key(&user_id, &hash);
        known.insert(key.clone());
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding(
                "cloudSaveBlob",
                key,
                json!({ "userId": user_id, "hash": hash, "bytes": row.get::<i64, _>("size_in_bytes") }),
            ));
        }
    }

    /* A committed manifest referencing a hash with no blob row at all is the
       same failure one step earlier, and just as fatal to a restore. */
    let dangling = sqlx::query(
        "SELECT DISTINCT s.user_id, f.hash, s.id AS snapshot_id
         FROM cloud_save_snapshot_files f
         JOIN cloud_save_snapshots s ON s.id = f.snapshot_id
         WHERE s.status = 'committed'
           AND NOT EXISTS (
             SELECT 1 FROM cloud_save_blobs b
             WHERE b.user_id = s.user_id AND b.hash = f.hash
           )",
    )
    .fetch_all(&state.pool)
    .await?;
    for row in &dangling {
        let user_id: String = row.get("user_id");
        let hash: String = row.get("hash");
        missing.push(finding(
            "unregisteredBlob",
            crate::storage::cloud_save_blob_key(&user_id, &hash),
            json!({
                "userId": user_id,
                "hash": hash,
                "snapshotId": row.get::<String, _>("snapshot_id"),
            }),
        ));
    }

    let artifacts = sqlx::query(
        "SELECT id, user_id, artifact_length_in_bytes FROM artifacts WHERE is_uploaded = 1",
    )
    .fetch_all(&state.pool)
    .await?;
    for row in &artifacts {
        let id: String = row.get("id");
        let key = format!("artifacts/{id}.tar");
        known.insert(key.clone());
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding(
                "backup",
                key,
                json!({
                    "id": id,
                    "userId": row.get::<String, _>("user_id"),
                    "bytes": row.get::<i64, _>("artifact_length_in_bytes"),
                }),
            ));
        }
    }

    let saves = sqlx::query(
        "SELECT id, user_id, artifact_length_in_bytes FROM emulation_saves WHERE is_uploaded = 1",
    )
    .fetch_all(&state.pool)
    .await?;
    for row in &saves {
        let id: String = row.get("id");
        let key = format!("emulation-saves/{id}.bin");
        known.insert(key.clone());
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding(
                "emulationSave",
                key,
                json!({
                    "id": id,
                    "userId": row.get::<String, _>("user_id"),
                    "bytes": row.get::<i64, _>("artifact_length_in_bytes"),
                }),
            ));
        }
    }

    let artwork_keys: Vec<Option<String>> =
        sqlx::query_scalar("SELECT storage_key FROM game_artwork WHERE storage_key IS NOT NULL")
            .fetch_all(&state.pool)
            .await?;
    for key in artwork_keys.into_iter().flatten() {
        known.insert(key.clone());
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding("artwork", key, json!({})));
        }
    }

    let souvenir_keys: Vec<String> =
        sqlx::query_scalar("SELECT image_key FROM souvenirs WHERE is_uploaded = 1")
            .fetch_all(&state.pool)
            .await?;
    for key in souvenir_keys {
        known.insert(key.clone());
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding("souvenir", key, json!({})));
        }
    }

    /* A reservation whose upload never arrived has no file to reconcile and
       isn't an orphan either — Maintenance sweeps those. Its key still counts
       as known so a partially-written file isn't reported as stray bytes. */
    let pending_souvenir_keys: Vec<String> =
        sqlx::query_scalar("SELECT image_key FROM souvenirs WHERE is_uploaded = 0")
            .fetch_all(&state.pool)
            .await?;
    known.extend(pending_souvenir_keys);

    /* Profile images and banners are reachable by URL, not by a per-file row;
       treat everything under images/ as accounted for rather than orphaned. */
    let banner_keys: Vec<Option<String>> =
        sqlx::query_scalar("SELECT banner_key FROM users WHERE banner_key IS NOT NULL")
            .fetch_all(&state.pool)
            .await?;
    for key in banner_keys.into_iter().flatten() {
        if tokio::fs::metadata(root.join(&key)).await.is_err() {
            missing.push(finding("banner", key, json!({})));
        }
    }

    // --- bytes no row points at ---------------------------------------------
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }

            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let key = relative.to_string_lossy().replace('\\', "/");

            /* Banners and avatars are reachable by URL with no per-file row
               to reconcile against (see above) — but custom artwork under
               images/artwork/ and souvenirs under images/souvenirs/ do have
               one, so they stay in the scan. A .uploading file is a transfer
               in flight, not an orphan. */
            let unreconciled = key.starts_with("images/")
                && !key.starts_with("images/artwork/")
                && !key.starts_with("images/souvenirs/");
            if unreconciled || key.ends_with(".uploading") {
                continue;
            }
            if !known.contains(&key) {
                orphan_bytes += meta.len();
                orphans.push(json!({ "key": key, "bytes": meta.len() }));
            }
        }
    }

    orphans.sort_by_key(|entry| std::cmp::Reverse(entry["bytes"].as_u64().unwrap_or(0)));

    let missing_count = missing.len();
    let orphan_count = orphans.len();
    let missing_bytes: u64 = missing
        .iter()
        .filter_map(|entry| entry["detail"]["bytes"].as_u64())
        .sum();

    Ok(Json(json!({
        "checkedAt": chrono::Utc::now().to_rfc3339(),
        "missing": missing,
        "missingCount": missing_count,
        "missingBytes": missing_bytes,
        "orphans": orphans,
        "orphanCount": orphan_count,
        "orphanBytes": orphan_bytes,
        "healthy": missing_count == 0 && orphan_count == 0,
    })))
}
