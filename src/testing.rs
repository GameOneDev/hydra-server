//! A real server on a scratch database, for the tests that need one.
//!
//! What the endpoints here do depends on rows other endpoints wrote, on the
//! settings in force and on what is already on disk. A stand-in for those
//! would only be testing the stand-in, so tests get the actual [`AppState`]
//! over a temporary SQLite file and storage directory, thrown away with the
//! test.

use crate::state::{AppState, RuntimeSettings};
use chrono::Utc;
use std::path::PathBuf;
use std::sync::Arc;

pub struct TestServer {
    pub state: AppState,
    dir: PathBuf,
}

impl TestServer {
    /// A migrated, empty server with one user, `alice`, and no quota.
    pub async fn start() -> Self {
        let dir = std::env::temp_dir().join(format!("hydra-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch directory");

        let mut config = crate::config::Config::for_test();
        config.data_dir = dir.clone();

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(config.database_path())
                    .create_if_missing(true),
            )
            .await
            .expect("scratch database");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");

        let settings = RuntimeSettings::from_config(&config);

        let state = AppState {
            pool,
            config: Arc::new(config),
            http: reqwest::Client::new(),
            token_cache: Default::default(),
            settings: Arc::new(tokio::sync::RwLock::new(settings)),
            started_at: Utc::now(),
            metrics: Default::default(),
            uploads: Default::default(),
            login_guard: Default::default(),
            presence: Default::default(),
        };

        let server = Self { state, dir };
        server.add_user("alice").await;
        server
    }

    pub async fn add_user(&self, id: &str) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, display_name, created_at, last_seen_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(id)
        .bind(&now)
        .bind(&now)
        .execute(&self.state.pool)
        .await
        .expect("a user to charge");
    }

    /// The launcher's identity for `id`, as the auth extractor would build it.
    pub fn user(&self, id: &str) -> crate::auth::CurrentUser {
        crate::auth::CurrentUser(crate::state::AuthenticatedUser {
            id: id.to_string(),
            username: Some(id.to_string()),
            display_name: id.to_string(),
            profile_image_url: None,
        })
    }

    /// The server-wide settings, as an operator would change them on the
    /// Settings screen.
    pub async fn settings(&self, edit: impl FnOnce(&mut RuntimeSettings)) {
        edit(&mut *self.state.settings.write().await);
    }

    /// One account's exception to those settings, as the panel saves it.
    pub async fn limits(&self, user_id: &str, overrides: crate::limits::Overrides) {
        assert!(
            crate::limits::save(&self.state, user_id, overrides)
                .await
                .expect("the override"),
            "no such user: {user_id}"
        );
    }

    pub async fn scalar<T>(&self, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Send + Unpin,
    {
        sqlx::query_scalar(sql)
            .fetch_one(&self.state.pool)
            .await
            .expect(sql)
    }

    pub async fn execute(&self, sql: &str) {
        sqlx::query(sql).execute(&self.state.pool).await.expect(sql);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
