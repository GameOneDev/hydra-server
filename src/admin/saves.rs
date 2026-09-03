//! Every stored save on the server, in one browsable list.
//!
//! Three storage generations coexist — V2 snapshots (launcher 4.1.0+), the
//! legacy per-backup tarballs, and emulation memory cards — and an operator
//! chasing "who is using all the space" should not have to know which is
//! which. They are unioned into one row shape here, with the per-kind detail
//! and actions hanging off it.

use super::{AdminSession, Paging};
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::{cloud_saves, storage};
use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/saves", get(list))
        .route("/admin/api/cloud-saves/{id}", get(snapshot).delete(delete_snapshot))
        .route("/admin/api/cloud-saves/{id}/files", get(snapshot_files))
        .route(
            "/admin/api/cloud-saves/{id}/files/{hash}/download",
            get(download_snapshot_file),
        )
        .route("/admin/api/artifacts/{id}", delete(delete_artifact))
        .route("/admin/api/artifacts/{id}/download", get(download_artifact))
        .route("/admin/api/artifacts/{id}/freeze", post(set_frozen))
        .route("/admin/api/emulation-saves/{id}", delete(delete_emulation_save))
        .route(
            "/admin/api/emulation-saves/{id}/download",
            get(download_emulation_save),
        )
}

/// The three kinds, normalised. `state` is the one word that says whether the
/// bytes are actually there: committed/uploaded, or pending.
const SAVES_UNION: &str = "
    SELECT 'cloud' AS kind, s.id, s.user_id, s.shop, s.object_id,
           s.total_size_in_bytes AS size_bytes, s.updated_at AS at, s.created_at,
           s.hostname, s.platform, s.status AS state, s.file_count, s.version,
           NULL AS label, NULL AS detail, 0 AS is_frozen, 0 AS share_count, 0 AS download_count
      FROM cloud_save_snapshots s
    UNION ALL
    SELECT 'legacy', a.id, a.user_id, a.shop, a.object_id,
           a.artifact_length_in_bytes, a.created_at, a.created_at,
           a.hostname, a.platform,
           CASE WHEN a.is_uploaded = 1 THEN 'uploaded' ELSE 'pending' END,
           NULL, NULL, a.label, a.download_option_title, a.is_frozen,
           (SELECT COUNT(*) FROM artifact_shares sh WHERE sh.artifact_id = a.id),
           a.download_count
      FROM artifacts a
    UNION ALL
    SELECT 'emulation', e.id, e.user_id, e.shop, e.object_id,
           e.artifact_length_in_bytes, e.updated_at, e.created_at,
           e.hostname, e.platform,
           CASE WHEN e.superseded_at IS NOT NULL THEN 'superseded'
                WHEN e.is_uploaded = 1 THEN 'uploaded' ELSE 'pending' END,
           NULL, NULL, COALESCE(e.label, e.file_name), e.emulator, 0, 0, 0
      FROM emulation_saves e
";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    /// `cloud`, `legacy`, `emulation`, or absent for all three.
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    shop: Option<String>,
    #[serde(default)]
    object_id: Option<String>,
    /// `pending` narrows to uploads that never finished.
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    per_page: Option<i64>,
}

/// GET /admin/api/saves — the unified browser, filtered and paginated.
async fn list(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let paging = Paging::new(query.page, query.per_page);

    /* Filters are positional so the same list of binds serves the count and
       the page query; every one of them is a bound value, never inlined. */
    let mut filters: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    /// Records a bound value and the clause that reads it, numbered by
    /// position so one bind list serves every query built below.
    fn filter(
        filters: &mut Vec<String>,
        binds: &mut Vec<String>,
        value: &str,
        clause: impl Fn(usize) -> String,
    ) {
        binds.push(value.to_string());
        filters.push(clause(binds.len()));
    }

    if let Some(kind) = query.r#type.as_deref().filter(|kind| *kind != "all") {
        filter(&mut filters, &mut binds, kind, |i| format!("x.kind = ?{i}"));
    }
    if let Some(user_id) = query.user_id.as_deref() {
        filter(&mut filters, &mut binds, user_id, |i| {
            format!("x.user_id = ?{i}")
        });
    }
    if let Some(shop) = query.shop.as_deref() {
        filter(&mut filters, &mut binds, shop, |i| format!("x.shop = ?{i}"));
    }
    if let Some(object_id) = query.object_id.as_deref() {
        filter(&mut filters, &mut binds, object_id, |i| {
            format!("x.object_id = ?{i}")
        });
    }
    if let Some(save_state) = query.state.as_deref() {
        filter(&mut filters, &mut binds, save_state, |i| {
            format!("x.state = ?{i}")
        });
    }
    if let Some(pattern) = super::like_pattern(query.q.as_deref()) {
        filter(&mut filters, &mut binds, &pattern, |i| {
            format!(
                "(g.name LIKE ?{i} ESCAPE '\\' OR x.object_id LIKE ?{i} ESCAPE '\\'
                  OR x.hostname LIKE ?{i} ESCAPE '\\' OR x.label LIKE ?{i} ESCAPE '\\'
                  OR u.display_name LIKE ?{i} ESCAPE '\\')"
            )
        });
    }

    let where_clause = if filters.is_empty() {
        "1 = 1".to_string()
    } else {
        filters.join(" AND ")
    };

    let from = format!(
        "FROM ({SAVES_UNION}) x
         LEFT JOIN users u ON u.id = x.user_id
         LEFT JOIN game_metadata g ON g.shop = x.shop AND g.object_id = x.object_id
         WHERE {where_clause}"
    );

    let count_sql = format!("SELECT COUNT(*) {from}");
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql);
    for value in &binds {
        count = count.bind(value);
    }
    let total = count.fetch_one(&state.pool).await?;

    let order = super::order_by(
        &[
            ("size", "x.size_bytes"),
            ("updated", "x.at"),
            ("created", "x.created_at"),
            ("game", "COALESCE(g.name, x.object_id) COLLATE NOCASE"),
            ("user", "u.display_name COLLATE NOCASE"),
        ],
        query.sort.as_deref(),
        query.dir.as_deref(),
        "x.at",
    );

    /* Every placeholder is numbered, paging included: mixing `?N` with bare
       `?` in one statement does not survive the round trip through the
       driver, and binds a filter value to LIMIT. */
    let (limit_slot, offset_slot) = (binds.len() + 1, binds.len() + 2);
    let sql = format!(
        "SELECT x.*, u.display_name, u.username, u.profile_image_url,
                g.name AS game_name, g.cover_url AS game_cover_url
         {from} ORDER BY {order} LIMIT ?{limit_slot} OFFSET ?{offset_slot}"
    );
    let mut rows = sqlx::query(&sql);
    for value in &binds {
        rows = rows.bind(value);
    }
    let rows = rows
        .bind(paging.per_page())
        .bind(paging.offset())
        .fetch_all(&state.pool)
        .await?;

    let saves: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "kind": row.get::<String, _>("kind"),
                "id": row.get::<String, _>("id"),
                "user": super::user_ref(row),
                "game": super::game_ref(row),
                "sizeBytes": row.get::<i64, _>("size_bytes"),
                "at": row.get::<String, _>("at"),
                "createdAt": row.get::<String, _>("created_at"),
                "hostname": row.get::<Option<String>, _>("hostname"),
                "platform": row.get::<Option<String>, _>("platform"),
                "state": row.get::<String, _>("state"),
                "fileCount": row.get::<Option<i64>, _>("file_count"),
                "version": row.get::<Option<i64>, _>("version"),
                "label": row.get::<Option<String>, _>("label"),
                "detail": row.get::<Option<String>, _>("detail"),
                "isFrozen": row.get::<i64, _>("is_frozen") != 0,
                "shareCount": row.get::<i64, _>("share_count"),
                "downloadCount": row.get::<i64, _>("download_count"),
            })
        })
        .collect();

    /* Totals for the current filter, not just the current page — "3 of 412
       shown, 84.2 GB matched" is the number an operator is actually after. */
    let sums_sql = format!(
        "SELECT COALESCE(SUM(x.size_bytes), 0) AS bytes, x.kind, COUNT(*) AS items {from} GROUP BY x.kind"
    );
    let mut sums = sqlx::query(&sums_sql);
    for value in &binds {
        sums = sums.bind(value);
    }
    let sums = sums.fetch_all(&state.pool).await?;

    let mut envelope = paging.envelope(saves, total);
    envelope["byKind"] = json!(sums
        .iter()
        .map(|row| json!({
            "kind": row.get::<String, _>("kind"),
            "items": row.get::<i64, _>("items"),
            "bytes": row.get::<i64, _>("bytes"),
        }))
        .collect::<Vec<_>>());
    envelope["matchedBytes"] = json!(sums
        .iter()
        .map(|row| row.get::<i64, _>("bytes"))
        .sum::<i64>());

    Ok(Json(envelope))
}

// ---------------------------------------------------------------------------
// Cloud Save V2 snapshots
// ---------------------------------------------------------------------------

/// GET /admin/api/cloud-saves/{id} — one snapshot in full, including the
/// variant and custom-path metadata the launcher sent with it.
async fn snapshot(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row = sqlx::query(
        "SELECT s.*, u.display_name, u.username, u.profile_image_url,
                g.name AS game_name, g.cover_url AS game_cover_url
         FROM cloud_save_snapshots s
         LEFT JOIN users u ON u.id = s.user_id
         LEFT JOIN game_metadata g ON g.shop = s.shop AND g.object_id = s.object_id
         WHERE s.id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("snapshot not found"))?;

    let custom_paths: Value =
        serde_json::from_str(&row.get::<String, _>("custom_path_raw_paths")).unwrap_or(json!([]));
    let variants: Value =
        serde_json::from_str(&row.get::<String, _>("variants")).unwrap_or(json!([]));

    Ok(Json(json!({
        "id": row.get::<String, _>("id"),
        "user": super::user_ref(&row),
        "game": super::game_ref(&row),
        "version": row.get::<i64, _>("version"),
        "status": row.get::<String, _>("status"),
        "fileCount": row.get::<i64, _>("file_count"),
        "sizeBytes": row.get::<i64, _>("total_size_in_bytes"),
        "aggregateHash": row.get::<String, _>("aggregate_hash"),
        "platform": row.get::<Option<String>, _>("platform"),
        "hostname": row.get::<Option<String>, _>("hostname"),
        "customPathRawPaths": custom_paths,
        "variants": variants,
        "createdAt": row.get::<String, _>("created_at"),
        "updatedAt": row.get::<String, _>("updated_at"),
    })))
}

/// GET /admin/api/cloud-saves/{id}/files — the manifest.
///
/// `stored` reports whether the blob is actually on disk: for a pending
/// snapshot it shows how far an in-flight upload got, and for a committed one
/// it exposes bytes lost underneath the database.
async fn snapshot_files(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let owner = snapshot_owner(&state, &id).await?;

    let rows = sqlx::query(
        "SELECT f.*, b.hash IS NOT NULL AS stored,
                (SELECT COUNT(*) FROM cloud_save_snapshot_files o
                  WHERE o.snapshot_id = f.snapshot_id AND o.hash = f.hash) AS copies
         FROM cloud_save_snapshot_files f
         LEFT JOIN cloud_save_blobs b ON b.user_id = ? AND b.hash = f.hash
         WHERE f.snapshot_id = ?
         ORDER BY f.raw_path, f.relative_path",
    )
    .bind(&owner)
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!(rows
        .iter()
        .map(|row| json!({
            "variantId": row.get::<String, _>("variant_id"),
            "rawPath": row.get::<String, _>("raw_path"),
            "relativePath": row.get::<String, _>("relative_path"),
            "hash": row.get::<String, _>("hash"),
            "sizeBytes": row.get::<i64, _>("size_in_bytes"),
            "lastModifiedAt": row.get::<String, _>("last_modified_at"),
            "stored": row.get::<i64, _>("stored") != 0,
            "copies": row.get::<i64, _>("copies"),
        }))
        .collect::<Vec<_>>())))
}

/// GET /admin/api/cloud-saves/{id}/files/{hash}/download — one file out of a
/// snapshot. The hash has to belong to the snapshot, so this can't be used to
/// read arbitrary blobs of an arbitrary user.
async fn download_snapshot_file(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path((id, hash)): Path<(String, String)>,
) -> ApiResult<Redirect> {
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT s.user_id FROM cloud_save_snapshots s
         JOIN cloud_save_snapshot_files f ON f.snapshot_id = s.id
         WHERE s.id = ? AND f.hash = ? LIMIT 1",
    )
    .bind(&id)
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?;
    let owner = owner.ok_or_else(|| ApiError::not_found("snapshot file not found"))?;

    let url = storage::sign_download_url(&state, &storage::cloud_save_blob_key(&owner, &hash));
    Ok(Redirect::temporary(&url))
}

/// DELETE /admin/api/cloud-saves/{id} — drops one snapshot and frees every
/// blob it alone was keeping alive.
async fn delete_snapshot(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let owner = snapshot_owner(&state, &id).await?;
    let before = storage::used_bytes(&state, &owner).await?;

    sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    cloud_saves::collect_orphan_blobs(&state, &owner).await?;

    let after = storage::used_bytes(&state, &owner).await?;

    crate::events::record(
        &state,
        Event::admin("admin.save.deleted", "Deleted a cloud save")
            .about(&owner)
            .detail(json!({ "snapshotId": id }))
            .size(before - after)
            .warning(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": before - after })))
}

async fn snapshot_owner(state: &AppState, id: &str) -> ApiResult<String> {
    sqlx::query_scalar("SELECT user_id FROM cloud_save_snapshots WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("snapshot not found"))
}

// ---------------------------------------------------------------------------
// Legacy backups
// ---------------------------------------------------------------------------

async fn delete_artifact(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let size: Option<i64> =
        sqlx::query_scalar("SELECT artifact_length_in_bytes FROM artifacts WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;
    let size = size.ok_or_else(|| ApiError::not_found("backup not found"))?;

    sqlx::query("DELETE FROM artifacts WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    storage::delete_object(&state, &format!("artifacts/{id}.tar")).await;

    crate::events::record(
        &state,
        Event::admin("admin.save.deleted", "Deleted a save backup")
            .detail(json!({ "artifactId": id }))
            .size(size)
            .warning(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": size })))
}

async fn download_artifact(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Redirect> {
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM artifacts WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("backup not found"));
    }

    let url = storage::sign_download_url(&state, &format!("artifacts/{id}.tar"));
    Ok(Redirect::temporary(&url))
}

#[derive(Deserialize)]
struct FreezeRequest {
    frozen: bool,
}

/// POST /admin/api/artifacts/{id}/freeze — a frozen backup is exempt from the
/// per-game limit, so this is how an operator pins a known-good save the
/// launcher would otherwise rotate away.
async fn set_frozen(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
    Json(payload): Json<FreezeRequest>,
) -> ApiResult<Json<Value>> {
    let result = sqlx::query("UPDATE artifacts SET is_frozen = ? WHERE id = ?")
        .bind(payload.frozen as i64)
        .bind(&id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("backup not found"));
    }

    crate::events::record(
        &state,
        Event::admin(
            if payload.frozen { "admin.save.frozen" } else { "admin.save.unfrozen" },
            if payload.frozen {
                "Froze a backup so the launcher can't rotate it away"
            } else {
                "Unfroze a backup"
            },
        )
        .detail(json!({ "artifactId": id })),
    )
    .await;

    Ok(Json(json!({ "ok": true, "isFrozen": payload.frozen })))
}

// ---------------------------------------------------------------------------
// Emulation saves
// ---------------------------------------------------------------------------

async fn delete_emulation_save(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let size: Option<i64> =
        sqlx::query_scalar("SELECT artifact_length_in_bytes FROM emulation_saves WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;
    let size = size.ok_or_else(|| ApiError::not_found("emulation save not found"))?;

    sqlx::query("DELETE FROM emulation_saves WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;
    storage::delete_object(&state, &format!("emulation-saves/{id}.bin")).await;

    crate::events::record(
        &state,
        Event::admin("admin.save.deleted", "Deleted an emulation save")
            .detail(json!({ "saveId": id }))
            .size(size)
            .warning(),
    )
    .await;

    Ok(Json(json!({ "ok": true, "freedBytes": size })))
}

async fn download_emulation_save(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(id): Path<String>,
) -> ApiResult<Redirect> {
    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM emulation_saves WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("emulation save not found"));
    }

    let url = storage::sign_download_url(&state, &format!("emulation-saves/{id}.bin"));
    Ok(Redirect::temporary(&url))
}
