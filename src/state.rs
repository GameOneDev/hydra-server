use crate::config::Config;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::collections::HashMap;
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

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    /// Official access token -> verified user, kept for a few minutes so we
    /// don't hit the official API on every request.
    pub token_cache: Arc<RwLock<HashMap<String, CachedUser>>>,
}
