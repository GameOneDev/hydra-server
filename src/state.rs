use crate::config::Config;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub id: String,
    pub username: Option<String>,
    pub display_name: String,
    pub profile_image_url: Option<String>,
}

pub struct CachedUser {
    pub user: AuthenticatedUser,
    pub cached_at: DateTime<Utc>,
}

/// Settings that can be changed at runtime from the admin panel. The
/// environment config provides the defaults; overrides live in the
/// `server_settings` table (see the `settings` module).
#[derive(Clone, Debug)]
pub struct RuntimeSettings {
    /// Max total stored bytes per user (0 = unlimited).
    pub max_bytes_per_user: u64,
    /// Max save backups kept per game per user.
    pub backups_per_game_limit: u32,
    /// Official user ids or usernames allowed on this server, lowercased.
    /// Empty = everyone with a valid official login.
    pub allowed_users: Vec<String>,
}

impl RuntimeSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_bytes_per_user: config.max_bytes_per_user,
            backups_per_game_limit: config.backups_per_game_limit,
            allowed_users: config.allowed_users.clone(),
        }
    }

    pub fn user_allowed(&self, id: &str, username: Option<&str>) -> bool {
        if self.allowed_users.is_empty() {
            return true;
        }

        let id = id.to_lowercase();
        let username = username.map(|u| u.to_lowercase());

        self.allowed_users
            .iter()
            .any(|allowed| *allowed == id || Some(allowed) == username.as_ref())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    /// Official access token -> verified user, kept for a few minutes so we
    /// don't hit the official API on every request.
    pub token_cache: Arc<RwLock<HashMap<String, CachedUser>>>,
    /// Admin-editable settings; see [`RuntimeSettings`].
    pub settings: Arc<RwLock<RuntimeSettings>>,
    /// Process start, for the admin panel's uptime display.
    pub started_at: DateTime<Utc>,
    /// Counters behind `/metrics`.
    pub metrics: Arc<crate::metrics::Counters>,
    /// Uploads currently streaming, so two of them are measured against each
    /// other and not only against what the database already knows.
    pub uploads: Arc<crate::storage::InFlightUploads>,
    /// Failed sign-ins per address, for the login lockout.
    pub login_guard: Arc<RwLock<crate::ratelimit::Guard>>,
    pub running_tasks: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Last time each user was seen calling, for the presence log. Memory
    /// only: the database keeps the durable answer, this keeps the hot path
    /// off it.
    pub presence: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
}
