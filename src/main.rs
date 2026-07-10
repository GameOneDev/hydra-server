mod achievements;
mod admin;
mod artifacts;
mod auth;
mod config;
mod emulation;
mod error;
mod games;
mod images;
mod settings;
mod shares;
mod sources;
mod state;
mod storage;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use config::Config;
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use state::AppState;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hydra_server=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();

    std::fs::create_dir_all(config.storage_dir()).expect("failed to create storage dir");

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(config.database_path())
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(10)),
        )
        .await
        .expect("failed to open database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");

    if config.admin_password.is_empty() {
        tracing::warn!("HYDRA_ADMIN_PASSWORD not set — admin panel is disabled");
    }

    let bind = config.bind.clone();
    let public_url = config.public_url.clone();

    let runtime_settings = settings::load(&pool, &config).await;

    let app_state = AppState {
        pool,
        config: Arc::new(config),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build http client"),
        token_cache: Arc::new(RwLock::new(HashMap::new())),
        settings: Arc::new(RwLock::new(runtime_settings)),
        started_at: chrono::Utc::now(),
    };

    let app = router(app_state.clone()).with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|err| panic!("failed to bind {bind}: {err}"));

    tracing::info!("hydra-server listening on {bind} (public url: {public_url})");
    tracing::info!("point the launcher's self-hosted cloud setting at {public_url}");

    axum::serve(listener, app).await.expect("server error");
}

fn router(_state: AppState) -> Router<AppState> {
    /* Save backups can be many GB — the storage routes stream to disk and
       must not be capped by the default body limit. */
    let storage_routes = Router::new()
        .route("/storage/{token}", put(storage::upload).get(storage::download))
        .layer(DefaultBodyLimit::disable());

    let api_routes = Router::new()
        .route(
            "/profile/games/artifacts",
            get(artifacts::list).post(artifacts::create),
        )
        .route(
            "/profile/games/artifacts/{id}",
            delete(artifacts::delete).patch(artifacts::rename),
        )
        .route(
            "/profile/games/artifacts/{id}/download",
            post(artifacts::download),
        )
        .route(
            "/profile/games/artifacts/shared-with-me",
            get(shares::shared_with_me),
        )
        .route(
            "/profile/games/artifacts/{id}/share",
            post(shares::share),
        )
        .route(
            "/profile/games/artifacts/{id}/share/{recipient_id}",
            delete(shares::unshare),
        )
        .route(
            "/profile/games/artifacts/{id}/shares",
            get(shares::list_shares),
        )
        .route("/profile/games/artifacts/{id}/freeze", put(artifacts::freeze))
        .route(
            "/profile/games/artifacts/{id}/unfreeze",
            put(artifacts::unfreeze),
        )
        .route("/profile/games/achievements", put(achievements::sync))
        .route(
            "/profile/games/achievements/{id}",
            delete(achievements::reset),
        )
        .route(
            "/profile/download-sources",
            get(sources::list).post(sources::add).delete(sources::remove),
        )
        .route("/profile/emulation-saves", get(emulation::list))
        .route(
            "/profile/emulation-saves/upload-url",
            post(emulation::create_upload_url),
        )
        .route(
            "/profile/emulation-saves/{id}",
            put(emulation::update).delete(emulation::delete),
        )
        .route("/profile/emulation-saves/{id}/commit", post(emulation::commit))
        .route(
            "/profile/emulation-saves/{id}/download-url",
            post(emulation::download_url),
        )
        .route("/presigned-urls/{type}", post(images::presign))
        .route("/profile/stats/{user_id}", get(achievements::user_stats))
        .route("/profile/banners/{user_id}", get(images::get_banner))
        .route("/profile/banner", delete(images::delete_banner))
        .route("/images/{*path}", get(images::serve))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));

    Router::new()
        .route("/health", get(health))
        .merge(api_routes)
        .merge(storage_routes)
        .merge(admin::router())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "name": "hydra-server",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
