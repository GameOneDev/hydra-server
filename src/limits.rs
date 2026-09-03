//! What one account is allowed, and what the server may delete on its behalf.
//!
//! The settings in [`crate::settings`] are the server's answer for everyone.
//! This module is the exception for one person: the operator gives a user a
//! bigger quota, or more backups per game, or tells the server to stop
//! deleting their saves — and everything else keeps the server-wide value.
//!
//! Overrides live on the `users` row as three nullable columns, where NULL
//! means "ask the server". [`Overrides::resolve`] is the one place that
//! decision is made, so a caller can only get the effective limit by going
//! through it.

use crate::state::{AppState, RuntimeSettings};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

/// The limits in force for one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Max total stored bytes (0 = unlimited).
    pub max_bytes_per_user: u64,
    /// Max legacy save backups kept per game.
    pub backups_per_game_limit: u32,
    /// Whether the server may delete this user's saves on its own — the older
    /// cloud save a commit replaces, the older emulation save in a slot.
    pub auto_delete_saves: bool,
}

/// What the panel saved for one account. `None` everywhere is an account
/// running on the server's own settings.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Overrides {
    #[serde(default)]
    pub max_bytes_per_user: Option<u64>,
    #[serde(default)]
    pub backups_per_game_limit: Option<u32>,
    #[serde(default)]
    pub auto_delete_saves: Option<bool>,
}

impl Overrides {
    /// Reads the three columns off a `users` row. Rows selected with `u.*`
    /// carry them, so a listing resolves its limits without a second query.
    pub fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        Self {
            max_bytes_per_user: row
                .try_get::<Option<i64>, _>("max_bytes_override")
                .ok()
                .flatten()
                .map(|bytes| bytes.max(0) as u64),
            backups_per_game_limit: row
                .try_get::<Option<i64>, _>("backups_per_game_override")
                .ok()
                .flatten()
                .map(|limit| limit.clamp(1, u32::MAX as i64) as u32),
            auto_delete_saves: row
                .try_get::<Option<i64>, _>("auto_delete_saves_override")
                .ok()
                .flatten()
                .map(|flag| flag != 0),
        }
    }

    pub fn any(&self) -> bool {
        *self != Self::default()
    }

    /// The server's settings, with whatever this account overrides applied.
    pub fn resolve(&self, defaults: &RuntimeSettings) -> Limits {
        Limits {
            max_bytes_per_user: self
                .max_bytes_per_user
                .unwrap_or(defaults.max_bytes_per_user),
            backups_per_game_limit: self
                .backups_per_game_limit
                .unwrap_or(defaults.backups_per_game_limit),
            auto_delete_saves: self.auto_delete_saves.unwrap_or(defaults.auto_delete_saves),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "maxBytesPerUser": self.max_bytes_per_user,
            "backupsPerGameLimit": self.backups_per_game_limit,
            "autoDeleteSaves": self.auto_delete_saves,
        })
    }
}

impl From<&RuntimeSettings> for Limits {
    /// The limits an account with no overrides of its own runs on.
    fn from(settings: &RuntimeSettings) -> Self {
        Overrides::default().resolve(settings)
    }
}

impl Limits {
    pub fn json(&self) -> Value {
        json!({
            "maxBytesPerUser": self.max_bytes_per_user,
            "backupsPerGameLimit": self.backups_per_game_limit,
            "autoDeleteSaves": self.auto_delete_saves,
        })
    }
}

/// The overrides saved for one account, or none for an id with no row — an
/// upload token outliving the user it was signed for, say. Callers get the
/// server's own settings in that case rather than an error, since a missing
/// user is not a reason to hand out an unlimited quota.
pub async fn overrides(state: &AppState, user_id: &str) -> Result<Overrides, sqlx::Error> {
    let row = sqlx::query(
        "SELECT max_bytes_override, backups_per_game_override, auto_delete_saves_override
         FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;

    Ok(row.as_ref().map(Overrides::from_row).unwrap_or_default())
}

/// The limits in force for one account: the server's settings, overridden.
pub async fn for_user(state: &AppState, user_id: &str) -> Result<Limits, sqlx::Error> {
    let overrides = overrides(state, user_id).await?;
    let defaults = state.settings.read().await.clone();
    Ok(overrides.resolve(&defaults))
}

/// Replaces every override for one account at once: a field left `None` goes
/// back to the server's value. Returns false when there is no such user.
pub async fn save(
    state: &AppState,
    user_id: &str,
    overrides: Overrides,
) -> Result<bool, sqlx::Error> {
    let max_bytes_override: Option<i64> = overrides
        .max_bytes_per_user
        .map(i64::try_from)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    let result = sqlx::query(
        "UPDATE users SET
            max_bytes_override = ?,
            backups_per_game_override = ?,
            auto_delete_saves_override = ?
         WHERE id = ?",
    )
    .bind(max_bytes_override)
    .bind(overrides.backups_per_game_limit.map(|limit| limit as i64))
    .bind(overrides.auto_delete_saves.map(i64::from))
    .bind(user_id)
    .execute(&state.pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// SQL for the quota a `users` row is measured against: that row's override,
/// or `server_default` where it has none.
///
/// The panel counts accounts sitting at their quota in one statement, and it
/// has to apply the same overrides the upload path does — so which column
/// carries an override is stated here and nowhere else. Both arguments are
/// literals from the caller, never anything from a request.
pub fn quota_expr(alias: &str, server_default: &str) -> String {
    format!("COALESCE({alias}.max_bytes_override, {server_default})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn defaults() -> RuntimeSettings {
        let mut config = Config::for_test();
        config.max_bytes_per_user = 1_000;
        config.backups_per_game_limit = 10;
        config.auto_delete_saves = true;
        RuntimeSettings::from_config(&config)
    }

    #[test]
    fn an_account_with_no_overrides_gets_the_server_settings() {
        let resolved = Overrides::default().resolve(&defaults());

        assert_eq!(resolved.max_bytes_per_user, 1_000);
        assert_eq!(resolved.backups_per_game_limit, 10);
        assert!(resolved.auto_delete_saves);
        assert!(!Overrides::default().any());
    }

    /// Each override stands alone: raising one person's quota must not also
    /// hand them the default number of backups when the server changes it.
    #[test]
    fn each_override_replaces_only_its_own_setting() {
        let overrides = Overrides {
            max_bytes_per_user: Some(50),
            ..Overrides::default()
        };
        let resolved = overrides.resolve(&defaults());

        assert_eq!(resolved.max_bytes_per_user, 50);
        assert_eq!(resolved.backups_per_game_limit, 10);
        assert!(overrides.any());
    }

    /// Zero is a value, not an absence: a per-user zero means unlimited for
    /// that account even while the server has a quota, and `false` turns off
    /// automatic deletion for one person on a server that does delete.
    #[test]
    fn a_falsy_override_is_still_an_override() {
        let overrides = Overrides {
            max_bytes_per_user: Some(0),
            auto_delete_saves: Some(false),
            ..Overrides::default()
        };
        let resolved = overrides.resolve(&defaults());

        assert_eq!(resolved.max_bytes_per_user, 0);
        assert!(!resolved.auto_delete_saves);
    }
}
