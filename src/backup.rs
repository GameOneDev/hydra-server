//! Database backups.
//!
//! The save files on disk are content-addressed and easy to copy with any
//! tool; the database is the irreplaceable part. Lose it and every blob
//! becomes an unreferenced file nobody can map back to a game — so this
//! writes consistent point-in-time copies of it, on a schedule and on demand.
//!
//! `VACUUM INTO` is the mechanism: SQLite produces a fully consistent copy of
//! a live database without blocking writers, which a file copy of a WAL-mode
//! database cannot promise.
//!
//! The timing is [`crate::schedule`]'s: this module owns what a backup *is*,
//! and the schedule owns when one is taken. Its cadence starts from
//! `HYDRA_BACKUP_INTERVAL_HOURS`, so nothing changes for a server whose
//! operator never opens the panel.

use crate::events::Event;
use crate::state::AppState;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// One backup on disk.
pub struct Backup {
    pub name: String,
    pub bytes: u64,
    pub created_at: String,
}

pub fn backup_json(backup: &Backup) -> Value {
    json!({
        "name": backup.name,
        "bytes": backup.bytes,
        "createdAt": backup.created_at,
    })
}

/// Backups are plain files in one directory; the name is the only identifier,
/// so it has to be impossible to point at anything else with one.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.ends_with(".db")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub async fn list(state: &AppState) -> Vec<Backup> {
    let dir = state.config.backup_dir();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return Vec::new();
    };

    let mut backups = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_valid_name(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        /* Fixed-width so the sort below is a plain string comparison. */
        let created_at = meta
            .modified()
            .ok()
            .map(|time| {
                chrono::DateTime::<Utc>::from(time)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            })
            .unwrap_or_default();

        backups.push(Backup {
            name,
            bytes: meta.len(),
            created_at,
        });
    }

    /* Newest first: the one an operator wants is almost always the last one.
       By timestamp rather than name — an uploaded file's name doesn't sort
       against a scheduled one's, and this order also decides what prune
       deletes. */
    backups.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.name.cmp(&a.name)));
    backups
}

pub fn path_for(state: &AppState, name: &str) -> Option<PathBuf> {
    is_valid_name(name).then(|| state.config.backup_dir().join(name))
}

/// Writes a new backup and prunes the oldest beyond the keep limit.
pub async fn create(state: &AppState, reason: &str) -> Result<Backup, String> {
    create_keeping(state, reason, None).await
}

/// `protect` names a backup the prune must not touch — the pre-restore backup
/// pushes the count over the keep limit, and the oldest file at that moment
/// can be the one the restore is about to read from.
async fn create_keeping(
    state: &AppState,
    reason: &str,
    protect: Option<&str>,
) -> Result<Backup, String> {
    let dir = state.config.backup_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| format!("cannot create the backup directory: {err}"))?;

    let (name, path) = free_name(&dir, &format!("hydra-{}", Utc::now().format("%Y%m%d-%H%M%S")))
        .await
        .ok_or_else(|| "too many backups taken in the same second".to_string())?;

    /* Refuse rather than fill the disk: a backup that leaves no room for the
       next upload has traded one failure for a worse one. */
    if let (Some(free), Ok(meta)) = (
        free_disk_bytes(&state.config.data_dir),
        tokio::fs::metadata(state.config.database_path()).await,
    ) {
        if free < meta.len().saturating_mul(2) {
            return Err(format!(
                "not enough free space — the database is {} and only {} is free",
                meta.len(),
                free
            ));
        }
    }

    sqlx::query("VACUUM INTO ?")
        .bind(path.to_string_lossy().as_ref())
        .execute(&state.pool)
        .await
        .map_err(|err| format!("backup failed: {err}"))?;

    let bytes = tokio::fs::metadata(&path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);

    let pruned = prune(state, protect).await;

    crate::events::record(
        state,
        Event::system("system.backup", format!("Database backed up to {name}"))
            .detail(json!({ "name": name, "bytes": bytes, "reason": reason, "pruned": pruned }))
            .size(bytes as i64),
    )
    .await;

    Ok(Backup {
        name,
        bytes,
        created_at: Utc::now().to_rfc3339(),
    })
}

/// Finds an unused file name for `stem`, suffixing it if the second-resolution
/// timestamp is already taken.
///
/// `VACUUM INTO` refuses to overwrite, so two backups in the same second would
/// otherwise fail — and one of those two is the safety backup a restore takes
/// right after the operator clicked "Back up now".
async fn free_name(dir: &Path, stem: &str) -> Option<(String, PathBuf)> {
    for attempt in 0..100 {
        let name = if attempt == 0 {
            format!("{stem}.db")
        } else {
            format!("{stem}-{}.db", attempt + 1)
        };
        let path = dir.join(&name);
        if tokio::fs::metadata(&path).await.is_err() {
            return Some((name, path));
        }
    }
    None
}

/// What a restore did, so the panel can say more than "done".
pub struct RestoreReport {
    pub tables: usize,
    pub rows: i64,
    /// The backup taken of the pre-restore state, so this is reversible too.
    pub safety_backup: String,
}

/// Replaces the live database's contents with a backup's.
///
/// Not a file swap: the pool has open connections and SQLite is mid-WAL, so
/// overwriting the file underneath would corrupt exactly the thing being
/// recovered. Instead the backup is attached and every table's rows are
/// swapped inside one transaction — atomic to every reader, and the process
/// keeps running with no restart and no supervisor required.
///
/// Save files on disk are deliberately untouched. Restoring an older database
/// can therefore leave rows whose bytes were already garbage collected, and
/// files no row points at any more; the caller is told to run the integrity
/// scan, which reconciles both directions.
pub async fn restore(state: &AppState, name: &str) -> Result<RestoreReport, String> {
    use sqlx::Connection;

    let path = path_for(state, name).ok_or_else(|| "invalid backup name".to_string())?;
    tokio::fs::metadata(&path)
        .await
        .map_err(|_| "backup not found".to_string())?;

    let tables = verify(state, &path).await?;

    /* Before overwriting anything: a backup of what is about to be replaced.
       Restoring the wrong file should cost a click, not the server. */
    let safety = create_keeping(state, "pre-restore", Some(name)).await?;

    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|err| format!("cannot open the database: {err}"))?;

    /* One connection for the whole operation: ATTACH is per-connection, and
       the swap has to happen inside a single transaction. */
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *connection)
        .await
        .map_err(|err| err.to_string())?;

    sqlx::query("ATTACH DATABASE ? AS backup")
        .bind(path.to_string_lossy().as_ref())
        .execute(&mut *connection)
        .await
        .map_err(|err| format!("cannot open the backup: {err}"))?;

    let swap = async {
        let mut transaction = connection
            .begin()
            .await
            .map_err(|err| format!("cannot start the restore: {err}"))?;

        let mut rows = 0i64;
        for table in &tables {
            /* Table names come from the database's own schema, never from the
               request, and are quoted regardless. */
            sqlx::query(&format!("DELETE FROM main.\"{table}\""))
                .execute(&mut *transaction)
                .await
                .map_err(|err| format!("clearing {table}: {err}"))?;

            let result = sqlx::query(&format!(
                "INSERT INTO main.\"{table}\" SELECT * FROM backup.\"{table}\""
            ))
            .execute(&mut *transaction)
            .await
            .map_err(|err| format!("restoring {table}: {err}"))?;

            rows += result.rows_affected() as i64;
        }

        transaction
            .commit()
            .await
            .map_err(|err| format!("committing the restore: {err}"))?;

        Ok::<i64, String>(rows)
    }
    .await;

    /* Detach and re-arm foreign keys whether or not the swap worked — a
       failed restore must not leave the connection in a strange state. */
    let _ = sqlx::query("DETACH DATABASE backup")
        .execute(&mut *connection)
        .await;
    let _ = sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *connection)
        .await;
    drop(connection);

    let rows = swap?;

    /* Settings and cached tokens both came from the database that just went
       away. */
    let reloaded = crate::settings::load(&state.pool, &state.config).await;
    *state.settings.write().await = reloaded;
    state.token_cache.write().await.clear();

    crate::events::record(
        state,
        Event::admin(
            "admin.backup.restored",
            format!("Restored the database from {name}"),
        )
        .detail(json!({
            "name": name,
            "tables": tables.len(),
            "rows": rows,
            "safetyBackup": safety.name,
        }))
        .critical(),
    )
    .await;

    tracing::warn!("database restored from {name} ({rows} rows across {} tables)", tables.len());

    Ok(RestoreReport {
        tables: tables.len(),
        rows,
        safety_backup: safety.name,
    })
}

/// Checks a file really is one of our backups, and returns the tables to swap.
///
/// The schema has to match exactly: a backup from before a migration has
/// different columns, and `INSERT … SELECT *` would either fail loudly or —
/// worse — line the wrong columns up. Refusing with an explanation beats
/// either.
async fn verify(state: &AppState, path: &Path) -> Result<Vec<String>, String> {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, SqliteConnection};

    let mut probe = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .create_if_missing(false),
    )
    .await
    .map_err(|_| "that file isn't a readable SQLite database".to_string())?;

    let backup_migrations: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE success = 1 ORDER BY version",
    )
    .fetch_all(&mut probe)
    .await
    .map_err(|_| "that database has no migration history — it isn't a hydra-server backup".to_string())?;

    let backup_tables: Vec<String> = sqlx::query_scalar(TABLE_QUERY)
        .fetch_all(&mut probe)
        .await
        .map_err(|err| err.to_string())?;

    let _ = probe.close().await;

    let live_migrations: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE success = 1 ORDER BY version",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|err| err.to_string())?;

    if backup_migrations != live_migrations {
        let newest_backup = backup_migrations.last().copied().unwrap_or(0);
        let newest_live = live_migrations.last().copied().unwrap_or(0);
        return Err(format!(
            "schema mismatch: the backup is at migration {newest_backup} and this server is at {newest_live}. \
             Restore it by stopping the server and putting the file in place of the database instead — \
             the migrations will then run against it on startup."
        ));
    }

    let live_tables: Vec<String> = sqlx::query_scalar(TABLE_QUERY)
        .fetch_all(&state.pool)
        .await
        .map_err(|err| err.to_string())?;

    let missing: Vec<&String> = live_tables
        .iter()
        .filter(|table| !backup_tables.contains(table))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the backup is missing table(s): {}",
            missing
                .iter()
                .map(|table| table.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(live_tables)
}

/// Every table the application owns. `_sqlx_migrations` stays as it is: the
/// schema is verified to match, and the live row is the one that describes
/// the file actually on disk.
const TABLE_QUERY: &str = "SELECT name FROM sqlite_master
     WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'
     ORDER BY name";

/// Stores an uploaded backup file after checking it is one.
///
/// The case this exists for: the database is gone, the server came back with
/// an empty one, and the only copy is on someone's laptop.
pub async fn store_upload(state: &AppState, bytes: &[u8]) -> Result<Backup, String> {
    if !bytes.starts_with(b"SQLite format 3\0") {
        return Err("that file isn't a SQLite database".to_string());
    }

    let dir = state.config.backup_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| format!("cannot create the backup directory: {err}"))?;

    let (name, path) = free_name(
        &dir,
        &format!("hydra-uploaded-{}", Utc::now().format("%Y%m%d-%H%M%S")),
    )
    .await
    .ok_or_else(|| "too many uploads in the same second".to_string())?;

    tokio::fs::write(&path, bytes)
        .await
        .map_err(|err| format!("cannot write the file: {err}"))?;

    /* Verify after writing so the operator gets the real reason it can't be
       used, then clean up rather than leave an unusable file lying around. */
    if let Err(error) = verify(state, &path).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }

    crate::events::record(
        state,
        Event::admin("admin.backup.uploaded", format!("Uploaded backup {name}"))
            .detail(json!({ "name": name }))
            .size(bytes.len() as i64),
    )
    .await;

    Ok(Backup {
        name,
        bytes: bytes.len() as u64,
        created_at: Utc::now().to_rfc3339(),
    })
}

/// Deletes the oldest backups beyond `backup_keep`, except `protect`. Returns
/// how many went.
async fn prune(state: &AppState, protect: Option<&str>) -> usize {
    let keep = state.config.backup_keep.max(1);
    let backups = list(state).await;

    let mut pruned = 0;
    for backup in backups.into_iter().skip(keep) {
        if Some(backup.name.as_str()) == protect {
            continue;
        }
        if let Some(path) = path_for(state, &backup.name) {
            if tokio::fs::remove_file(&path).await.is_ok() {
                pruned += 1;
            }
        }
    }

    pruned
}

/// Free bytes on the volume holding `path`.
///
/// The panel reports what this server stores; without this it cannot report
/// what is left, which is the number that decides whether the next upload
/// succeeds.
#[cfg(unix)]
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    /* SAFETY: c_path is a valid NUL-terminated string and stat is a live,
       correctly sized statvfs the call only writes into. */
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0;
    /* Field widths differ between libc targets; the casts keep this
       compiling on all of them. */
    #[allow(clippy::unnecessary_cast)]
    ok.then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_disk_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Total bytes on the volume holding `path`.
#[cfg(unix)]
pub fn total_disk_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    /* SAFETY: as above. */
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0;
    #[allow(clippy::unnecessary_cast)]
    ok.then(|| stat.f_blocks as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn total_disk_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two backups can land in the same second — a manual one, then the
    /// safety backup a restore takes moments later. `VACUUM INTO` refuses to
    /// overwrite, so the second name has to be different, and still has to
    /// pass the name guard.
    #[tokio::test]
    async fn same_second_backups_get_distinct_names() {
        let dir = std::env::temp_dir().join(format!("hydra-backup-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let (first, first_path) = free_name(&dir, "hydra-20260811-224117").await.unwrap();
        assert_eq!(first, "hydra-20260811-224117.db");
        tokio::fs::write(&first_path, b"").await.unwrap();

        let (second, second_path) = free_name(&dir, "hydra-20260811-224117").await.unwrap();
        assert_eq!(second, "hydra-20260811-224117-2.db");
        assert!(is_valid_name(&second));
        tokio::fs::write(&second_path, b"").await.unwrap();

        let (third, _) = free_name(&dir, "hydra-20260811-224117").await.unwrap();
        assert_eq!(third, "hydra-20260811-224117-3.db");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// A backup name reaches the filesystem, so the guard is the only thing
    /// standing between a path parameter and an arbitrary file.
    #[test]
    fn only_plain_backup_names_are_accepted() {
        assert!(is_valid_name("hydra-20260811-102030.db"));

        assert!(!is_valid_name("../../etc/passwd"));
        assert!(!is_valid_name("nested/path.db"));
        assert!(!is_valid_name("hydra..db"));
        assert!(!is_valid_name("hydra-20260811.sqlite"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name(&format!("{}.db", "a".repeat(80))));
    }
}
