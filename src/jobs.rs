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
        description: "Look up names and cover art again for every game the panel can only show as a raw shop id. One network round trip per game, so it takes a batch at a time, least recently tried first, and works a long backlog down over several runs.",
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

        let own = timer(
            self.default_every,
            self.default_unit,
            self.default_at_minute,
        );

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

/// How many store lookups one run makes. Each is a network round trip, so a
/// long backlog is worked through over several runs instead of one burst.
const METADATA_BATCH: usize = 50;

async fn refresh_metadata(state: &AppState) -> ApiResult<Value> {
    /* Least recently attempted first, games never attempted at all ahead of
    those: a fixed batch off an unordered query would re-ask the same ids
    every run and never reach the rest of the backlog.

    All of them rather than a batch-sized page, because what the run reports
    is the point of this job — an id pair is two short strings, and the batch
    below is what bounds the network. */
    let unnamed: Vec<(String, String)> = sqlx::query_as(&format!(
        "SELECT t.shop, t.object_id
         FROM ({known}) t
         LEFT JOIN game_metadata g ON g.shop = t.shop AND g.object_id = t.object_id
         WHERE {unresolved}
         ORDER BY COALESCE(g.fetched_at, '') ASC, t.shop ASC, t.object_id ASC",
        known = games::KNOWN_GAME_IDS,
        unresolved = games::unresolved_name("g.name"),
    ))
    .fetch_all(&state.pool)
    .await?;

    /* Ids no store answers for — a shop with no public endpoint, or an id
    that isn't a Steam app id. Asking again would spend the batch to learn
    nothing, so they are reported rather than retried. */
    let (lookupable, unsupported): (Vec<_>, Vec<_>) = unnamed
        .iter()
        .partition(|(shop, object_id)| games::is_lookupable(shop, object_id));

    let batch = &lookupable[..lookupable.len().min(METADATA_BATCH)];
    let mut resolved = 0usize;
    for (shop, object_id) in batch {
        if games::refresh(state, shop, object_id).await.name.is_some() {
            resolved += 1;
        }
    }

    let waiting = lookupable.len() - batch.len();
    let remaining = unnamed.len() - resolved;

    let summary = if unnamed.is_empty() {
        "Every game already has a name.".to_string()
    } else if remaining == 0 {
        format!(
            "Looked up {} game(s), resolved {resolved}. Every game has a name now.",
            batch.len()
        )
    } else {
        let mut summary = format!(
            "Looked up {} game(s), resolved {resolved}. {remaining} still without a name.",
            batch.len()
        );
        if waiting > 0 {
            summary.push_str(&format!(" {waiting} more to try on the next run."));
        }
        if !unsupported.is_empty() {
            summary.push_str(&format!(
                " {} of those are ids no public store answers for.",
                unsupported.len()
            ));
        }
        summary
    };

    Ok(json!({
        "summary": summary,
        "attempted": batch.len(),
        "resolved": resolved,
        "unnamed": unnamed.len(),
        "remaining": remaining,
        "waiting": waiting,
        "unsupported": unsupported.len(),
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
    use crate::testing::TestServer;

    /// A game the server holds something for, in the table that something
    /// arrived in. Every shop here is one with no public lookup, so the job
    /// counts the game without making a network call.
    async fn a_game_from(server: &TestServer, table: &str, shop: &str, object_id: &str) {
        let now = Utc::now().to_rfc3339();
        let sql = match table {
            "game_achievements" => format!(
                "INSERT INTO game_achievements (user_id, remote_game_id, shop, object_id, updated_at)
                 VALUES ('alice', '{object_id}', '{shop}', '{object_id}', '{now}')"
            ),
            "souvenirs" => format!(
                "INSERT INTO souvenirs
                   (id, user_id, client_id, shop, object_id, image_key, captured_at,
                    created_at, updated_at)
                 VALUES ('s-{object_id}', 'alice', 'c-{object_id}', '{shop}', '{object_id}',
                         'k-{object_id}', 0, '{now}', '{now}')"
            ),
            "emulation_saves" => format!(
                "INSERT INTO emulation_saves
                   (id, user_id, platform, emulator, save_identity, shop, object_id,
                    created_at, updated_at)
                 VALUES ('e-{object_id}', 'alice', 'ps2', 'pcsx2', 'slot1', '{shop}',
                         '{object_id}', '{now}', '{now}')"
            ),
            "playtime_daily" => format!(
                "INSERT INTO playtime_daily (user_id, day, shop, object_id, seconds, updated_at)
                 VALUES ('alice', '2026-01-01', '{shop}', '{object_id}', 60, '{now}')"
            ),
            other => panic!("no seed for {other}"),
        };

        server.execute(&sql).await;
    }

    /// What the metadata cache already holds for a game.
    async fn a_cached_name(server: &TestServer, shop: &str, object_id: &str, name: &str) {
        server
            .execute(&format!(
                "INSERT INTO game_metadata (shop, object_id, name, fetched_at)
                 VALUES ('{shop}', '{object_id}', '{name}', '{}')",
                Utc::now().to_rfc3339()
            ))
            .await;
    }

    #[tokio::test]
    async fn the_refresh_counts_games_from_every_table_they_arrive_in() {
        let server = TestServer::start().await;

        /* None of these reach the games list through a cloud save, and the
        job used to look no further than the tables that do. */
        a_game_from(&server, "game_achievements", "epic", "achievements-only").await;
        a_game_from(&server, "souvenirs", "epic", "souvenirs-only").await;
        a_game_from(&server, "emulation_saves", "epic", "emulation-only").await;

        let report = refresh_metadata(&server.state).await.expect("the refresh");

        assert_eq!(report["unnamed"], 3);
        assert_eq!(report["unsupported"], 3);
        assert_ne!(
            report["summary"].as_str().expect("a summary"),
            "Every game already has a name.",
            "three games have no name"
        );
    }

    #[tokio::test]
    async fn a_blank_cached_name_is_no_name() {
        let server = TestServer::start().await;
        a_game_from(&server, "playtime_daily", "epic", "blank").await;
        a_cached_name(&server, "epic", "blank", "   ").await;

        let report = refresh_metadata(&server.state).await.expect("the refresh");

        /* The panel falls back to the object id for a blank name exactly as
        it does for a missing one, so the job has to agree with it. */
        assert_eq!(report["unnamed"], 1);
    }

    #[tokio::test]
    async fn a_named_game_is_left_alone() {
        let server = TestServer::start().await;
        a_game_from(&server, "playtime_daily", "epic", "named").await;
        a_cached_name(&server, "epic", "named", "A Game").await;

        let report = refresh_metadata(&server.state).await.expect("the refresh");

        assert_eq!(report["unnamed"], 0);
        assert_eq!(report["attempted"], 0);
        assert_eq!(report["summary"], "Every game already has a name.");
    }

    #[tokio::test]
    async fn ids_no_store_can_answer_for_are_reported_not_retried() {
        let server = TestServer::start().await;
        a_game_from(&server, "playtime_daily", "gog", "1234").await;
        a_game_from(&server, "playtime_daily", "steam", "not-an-app-id").await;

        let report = refresh_metadata(&server.state).await.expect("the refresh");

        /* Neither is a lookup this server can make, so neither costs a slot
        in the batch — and the summary says so instead of implying the next
        run might do better. */
        assert_eq!(report["attempted"], 0);
        assert_eq!(report["unsupported"], 2);
        assert!(
            report["summary"]
                .as_str()
                .expect("a summary")
                .contains("no public store answers for"),
            "{}",
            report["summary"]
        );
    }

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
        assert!(
            !find("delete-orphan-files")
                .expect("the file sweep")
                .schedulable
        );
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
        assert!(
            !job.default_schedule(&config).0,
            "0 hours means no scheduled backup"
        );
    }
}
