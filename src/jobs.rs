use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::triggers::{Trigger, Unit};
use crate::{cloud_saves, games};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use sqlx::Row;

pub const PENDING_TTL_HOURS: i64 = 24;

const HOUR: i64 = 60;

const SUNDAY: i64 = 6;

pub struct Job {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub schedulable: bool,
    pub danger: bool,
    pub default_every: i64,
    pub default_unit: Unit,
    pub default_at_minute: Option<i64>,
    pub default_enabled: bool,
}

pub const BACKUP: &str = "backup";

pub const JOBS: &[Job] = &[
    Job {
        id: BACKUP,
        title: "Back up the database",
        description: "Write a consistent copy of the database to the backup directory, then prune the oldest beyond the keep limit. The save files on disk are easy to copy with any tool; this is the part that maps them back to games and users.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: Some(3 * HOUR),
        default_enabled: true,
    },
    Job {
        id: "sweep-pending",
        title: "Sweep abandoned uploads",
        description: "Delete cloud save snapshots and souvenir captures left pending for more than 24h. Their blobs are freed with them.",
        schedulable: true,
        danger: false,
        default_every: 6,
        default_unit: Unit::Hour,
        default_at_minute: None,
        default_enabled: true,
    },
    Job {
        id: "gc-blobs",
        title: "Collect orphaned blobs",
        description: "Delete Cloud Save V2 blobs no manifest references any more, for every user. Runs automatically after each commit; run it here if a quota looks wrong.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: Some(4 * HOUR),
        default_enabled: true,
    },
    Job {
        id: "delete-retained-versions",
        title: "Delete retained older versions",
        description: "Delete old game saves except the latest one.",
        schedulable: true,
        danger: true,
        default_every: 1,
        default_unit: Unit::Week,
        default_at_minute: Some(5 * HOUR),
        default_enabled: false,
    },
    Job {
        id: "delete-orphan-files",
        title: "Delete orphaned files",
        description: "Remove files on disk that no database row points at. Review them on the Storage screen first — this cannot be undone.",
        schedulable: false,
        danger: true,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: None,
        default_enabled: false,
    },
    Job {
        id: "refresh-metadata",
        title: "Refresh game metadata",
        description: "Re-resolve names and cover art for games the store lookup never answered for. One network round trip per game, so it works through a bounded batch at a time.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: Some(5 * HOUR),
        default_enabled: false,
    },
    Job {
        id: "clear-token-cache",
        title: "Clear the token cache",
        description: "Force every launcher token to be re-validated against the official API on its next request.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: Some(2 * HOUR),
        default_enabled: false,
    },
    Job {
        id: "prune-events",
        title: "Prune old history",
        description: "Delete recorded events past the retention window set by HYDRA_EVENT_RETENTION_DAYS.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Day,
        default_at_minute: Some(3 * HOUR + 30),
        default_enabled: true,
    },
    Job {
        id: "vacuum",
        title: "Compact the database",
        description: "Checkpoint the write-ahead log and VACUUM. Reclaims space after a large delete, and rewrites the whole file to do it — worth scheduling for a quiet hour rather than a busy one.",
        schedulable: true,
        danger: false,
        default_every: 1,
        default_unit: Unit::Week,
        default_at_minute: Some(4 * HOUR + 30),
        default_enabled: false,
    },
];

pub fn find(id: &str) -> Option<&'static Job> {
    JOBS.iter().find(|job| job.id == id)
}

impl Job {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "description": self.description,
            "danger": self.danger,
            "schedulable": self.schedulable,
        })
    }

    pub fn default_schedule(&self, config: &crate::config::Config) -> (bool, Vec<Trigger>) {
        let timer = |count, unit, at_minute| Trigger::Every {
            count,
            unit,
            at_minute,
            weekday: (unit == Unit::Week).then_some(SUNDAY),
            day: (unit == Unit::Month).then_some(1),
        };

        let own = timer(self.default_every, self.default_unit, self.default_at_minute);

        if self.id != BACKUP {
            return (self.default_enabled, vec![own]);
        }

        match config.backup_interval_hours {
            0 => (false, vec![own]),
            hours if hours % 24 == 0 => (
                true,
                vec![timer((hours / 24) as i64, Unit::Day, Some(3 * HOUR))],
            ),
            hours => (true, vec![timer(hours as i64, Unit::Hour, None)]),
        }
    }
}

pub async fn run(state: &AppState, id: &str, trigger: &str) -> ApiResult<Value> {
    match id {
        BACKUP => backup(state, trigger).await,
        "sweep-pending" => sweep_pending(state).await,
        "gc-blobs" => gc_blobs(state).await,
        "delete-retained-versions" => delete_retained_versions(state).await,
        "refresh-metadata" => refresh_metadata(state).await,
        "clear-token-cache" => clear_token_cache(state).await,
        "prune-events" => prune_events(state).await,
        "vacuum" => vacuum(state).await,
        other => Err(ApiError::bad_request(format!("unknown action: {other}"))),
    }
}

async fn backup(state: &AppState, trigger: &str) -> ApiResult<Value> {
    let backup = crate::backup::create(state, trigger)
        .await
        .map_err(ApiError::internal)?;

    Ok(json!({
        "summary": format!("Wrote {}.", backup.name),
        "name": backup.name,
        "bytes": backup.bytes,
    }))
}

async fn sweep_pending(state: &AppState) -> ApiResult<Value> {
    let cutoff = (Utc::now() - Duration::hours(PENDING_TTL_HOURS)).to_rfc3339();

    let stale = sqlx::query(
        "SELECT id, user_id FROM cloud_save_snapshots
         WHERE status = 'pending' AND created_at < ?",
    )
    .bind(&cutoff)
    .fetch_all(&state.pool)
    .await?;

    let mut owners: std::collections::HashSet<String> = Default::default();
    for row in &stale {
        let id: String = row.get("id");
        owners.insert(row.get("user_id"));

        sqlx::query("DELETE FROM cloud_save_snapshot_files WHERE snapshot_id = ?")
            .bind(&id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM cloud_save_snapshots WHERE id = ?")
            .bind(&id)
            .execute(&state.pool)
            .await?;
    }

    for user_id in &owners {
        cloud_saves::collect_orphan_blobs(state, user_id).await?;
    }

    let souvenirs = crate::souvenirs::sweep_abandoned(state, &cutoff).await?;
    let swept = stale.len() + souvenirs;

    Ok(json!({
        "summary": match swept {
            0 => "No abandoned uploads to sweep.".to_string(),
            n => format!(
                "Swept {n} abandoned upload(s): {} cloud save(s) across {} user(s), {souvenirs} souvenir(s).",
                stale.len(),
                owners.len()
            ),
        },
        "swept": swept,
    }))
}

async fn gc_blobs(state: &AppState) -> ApiResult<Value> {
    let users: Vec<String> = sqlx::query_scalar("SELECT DISTINCT user_id FROM cloud_save_blobs")
        .fetch_all(&state.pool)
        .await?;

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_save_blobs")
        .fetch_one(&state.pool)
        .await?;
    let bytes_before: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs")
            .fetch_one(&state.pool)
            .await?;

    for user_id in &users {
        cloud_saves::collect_orphan_blobs(state, user_id).await?;
    }

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_save_blobs")
        .fetch_one(&state.pool)
        .await?;
    let bytes_after: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs")
            .fetch_one(&state.pool)
            .await?;

    Ok(json!({
        "summary": match before - after {
            0 => "Nothing to collect — every blob is still referenced.".to_string(),
            n => format!("Freed {n} blob(s)."),
        },
        "freed": before - after,
        "freedBytes": bytes_before - bytes_after,
    }))
}

/// Deletes the older versions kept while automatic save deletion is off.
///
/// Turning the switch back on clears a game's retained versions on its next
/// sync, which never comes for a game nobody plays any more. This is the
/// sweep for those: the committed cloud save of each game and the current
/// save in each emulator slot are left exactly as they are.
async fn delete_retained_versions(state: &AppState) -> ApiResult<Value> {
    let snapshots =
        sqlx::query("SELECT id, user_id FROM cloud_save_snapshots WHERE status = 'superseded'")
            .fetch_all(&state.pool)
            .await?;

    let mut owners: std::collections::HashSet<String> = Default::default();
    for row in &snapshots {
        owners.insert(row.get("user_id"));
    }

    sqlx::query(
        "DELETE FROM cloud_save_snapshot_files
         WHERE snapshot_id IN (
           SELECT id FROM cloud_save_snapshots WHERE status = 'superseded'
         )",
    )
    .execute(&state.pool)
    .await?;
    sqlx::query("DELETE FROM cloud_save_snapshots WHERE status = 'superseded'")
        .execute(&state.pool)
        .await?;

    let saves: Vec<String> =
        sqlx::query_scalar("SELECT id FROM emulation_saves WHERE superseded_at IS NOT NULL")
            .fetch_all(&state.pool)
            .await?;

    for id in &saves {
        sqlx::query("DELETE FROM emulation_saves WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await?;
        crate::storage::delete_object(state, &format!("emulation-saves/{id}.bin")).await;
    }

    for user_id in &owners {
        cloud_saves::collect_orphan_blobs(state, user_id).await?;
    }

    let deleted = snapshots.len() + saves.len();

    Ok(json!({
        "summary": match deleted {
            0 => "Nothing retained — no older versions to delete.".to_string(),
            n => format!(
                "Deleted {n} retained version(s): {} cloud save(s), {} emulation save(s).",
                snapshots.len(),
                saves.len()
            ),
        },
        "deleted": deleted,
    }))
}

async fn refresh_metadata(state: &AppState) -> ApiResult<Value> {
    let pending: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT t.shop, t.object_id FROM (
             SELECT shop, object_id FROM cloud_save_snapshots
             UNION SELECT shop, object_id FROM artifacts
             UNION SELECT shop, object_id FROM playtime_daily
             UNION SELECT shop, object_id FROM game_artwork
         ) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         WHERE g.name IS NULL LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut resolved = 0usize;
    for (shop, object_id) in &pending {
        sqlx::query("DELETE FROM game_metadata WHERE shop = ? AND object_id = ?")
            .bind(shop)
            .bind(object_id)
            .execute(&state.pool)
            .await?;

        if games::resolve(state, shop, object_id).await.name.is_some() {
            resolved += 1;
        }
    }

    Ok(json!({
        "summary": match pending.len() {
            0 => "Every game already has a name.".to_string(),
            n => format!("Looked up {n} game(s), resolved {resolved}."),
        },
        "attempted": pending.len(),
        "resolved": resolved,
    }))
}

async fn clear_token_cache(state: &AppState) -> ApiResult<Value> {
    let cleared = {
        let mut cache = state.token_cache.write().await;
        let size = cache.len();
        cache.clear();
        size
    };

    Ok(json!({
        "summary": format!("Dropped {cleared} cached token(s)."),
        "cleared": cleared,
    }))
}

async fn prune_events(state: &AppState) -> ApiResult<Value> {
    let removed = crate::events::prune(state, state.config.event_retention_days)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;

    Ok(json!({
        "summary": match removed {
            0 => "Nothing past the retention window.".to_string(),
            n => format!("Pruned {n} event(s)."),
        },
        "pruned": removed,
    }))
}

async fn vacuum(state: &AppState) -> ApiResult<Value> {
    let before = database_bytes(state).await;

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&state.pool)
        .await?;
    sqlx::query("VACUUM").execute(&state.pool).await?;

    let after = database_bytes(state).await;

    Ok(json!({
        "summary": format!(
            "Database is {} after compacting.",
            if after < before { "smaller" } else { "unchanged" }
        ),
        "beforeBytes": before,
        "afterBytes": after,
    }))
}

pub async fn database_bytes(state: &AppState) -> u64 {
    let db_path = state.config.database_path();
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_job_has_a_distinct_id() {
        let mut ids: Vec<&str> = JOBS.iter().map(|job| job.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn only_jobs_that_can_run_unattended_are_schedulable() {
        assert!(!find("delete-orphan-files").expect("the file sweep").schedulable);
        assert!(find(BACKUP).expect("the backup job").schedulable);
        assert!(find("no-such-job").is_none());
    }

    #[test]
    fn the_backup_default_follows_the_environment() {
        let job = find(BACKUP).expect("the backup job");
        let mut config = crate::config::Config::for_test();

        config.backup_interval_hours = 24;
        let (enabled, triggers) = job.default_schedule(&config);
        assert!(enabled);
        assert_eq!(triggers[0].label(), "every day at 03:00 UTC");

        config.backup_interval_hours = 6;
        assert_eq!(job.default_schedule(&config).1[0].label(), "every 6 hours");

        config.backup_interval_hours = 48;
        assert_eq!(
            job.default_schedule(&config).1[0].label(),
            "every 2 days at 03:00 UTC"
        );

        config.backup_interval_hours = 0;
        assert!(!job.default_schedule(&config).0, "0 hours means no scheduled backup");
    }
}
