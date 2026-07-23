use crate::config::Config;
use crate::state::RuntimeSettings;
use chrono::Utc;
use sqlx::SqlitePool;

pub const MAX_BYTES_PER_USER: &str = "max_bytes_per_user";
pub const BACKUPS_PER_GAME_LIMIT: &str = "backups_per_game_limit";
pub const ALLOWED_USERS: &str = "allowed_users";
pub const AUTO_UPDATE: &str = "auto_update";

/// Effective settings: environment defaults overlaid with any overrides
/// saved from the admin panel (persisted in `server_settings`).
pub async fn load(pool: &SqlitePool, config: &Config) -> RuntimeSettings {
    let mut settings = RuntimeSettings::from_config(config);

    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM server_settings")
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    for (key, value) in rows {
        match key.as_str() {
            MAX_BYTES_PER_USER => {
                if let Ok(parsed) = value.parse() {
                    settings.max_bytes_per_user = parsed;
                }
            }
            BACKUPS_PER_GAME_LIMIT => {
                if let Ok(parsed) = value.parse::<u32>() {
                    if parsed > 0 {
                        settings.backups_per_game_limit = parsed;
                    }
                }
            }
            ALLOWED_USERS => settings.allowed_users = parse_allowed_users(&value),
            AUTO_UPDATE => settings.auto_update = value == "true",
            _ => {}
        }
    }

    settings
}

pub fn parse_allowed_users(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|user| user.trim().to_lowercase())
        .filter(|user| !user.is_empty())
        .collect()
}

pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO server_settings (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET
           value = excluded.value,
           updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;

    Ok(())
}

/// Drops every panel override so the environment values apply again.
pub async fn clear(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM server_settings").execute(pool).await?;
    Ok(())
}
