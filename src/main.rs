mod achievements;
mod admin;
mod artifacts;
mod artwork;
mod assets;
mod auth;
mod backup;
mod client_ip;
mod cloud_saves;
mod config;
mod emulation;
mod error;
mod events;
mod games;
mod images;
mod members;
mod metrics;
mod playtime;
mod portal;
mod presence;
mod ratelimit;
mod settings;
mod shares;
mod souvenirs;
mod sources;
mod state;
mod storage;
mod webhooks;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use std::net::SocketAddr;
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
        metrics: Arc::new(metrics::Counters::default()),
        login_guard: Arc::new(RwLock::new(Default::default())),
        presence: Arc::new(RwLock::new(Default::default())),
    };

    /* Backups and event pruning run in-process: the premise of this server is
       that it is one binary you start, not a binary plus a cron entry. */
    backup::spawn_scheduler(app_state.clone());

    events::record(
        &app_state,
        events::Event::system(
            "system.started",
            format!("Server started (v{})", env!("CARGO_PKG_VERSION")),
        ),
    )
    .await;

    let app = router(app_state.clone())
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            count_request,
        ))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|err| panic!("failed to bind {bind}: {err}"));

    tracing::info!("hydra-server listening on {bind} (public url: {public_url})");
    tracing::info!("point the launcher's self-hosted cloud setting at {public_url}");

    /* ConnectInfo gives the login lockout a real client address to key on. */
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}

/// Counts every response for `/metrics`.
async fn count_request(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    state.metrics.observe_status(response.status().as_u16());
    response
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
        /* Custom game artwork (Hydra Cloud's "Custom Image Sync"). The
           listing routes sit above the parameterised ones; static segments
           win in the router, so "artwork"/"artifacts" never get read as a
           shop name. */
        .route("/profile/games/artwork", get(artwork::list))
        .route("/profile/games/artwork/{user_id}", get(artwork::list_for_user))
        .route(
            "/profile/games/{shop}/{object_id}/artwork/{kind}/upload-url",
            post(artwork::upload_url),
        )
        .route(
            "/profile/games/{shop}/{object_id}/artwork/{kind}",
            put(artwork::save).delete(artwork::delete),
        )
        /* Cloud Save V2 (launcher 4.1.0+). Static segments before the
           parameterised /profile/games routes, same reason as artwork. */
        .route(
            "/profile/cloud-saves/snapshots",
            get(cloud_saves::list_snapshots).delete(cloud_saves::delete_snapshots),
        )
        .route(
            "/profile/cloud-saves/prepare-snapshot",
            post(cloud_saves::prepare_snapshot),
        )
        .route(
            "/profile/cloud-saves/commit-snapshot",
            post(cloud_saves::commit_snapshot),
        )
        .route(
            "/profile/cloud-saves/snapshot-restore-manifest",
            get(cloud_saves::restore_manifest),
        )
        .route(
            "/profile/cloud-saves/snapshot-download-urls",
            get(cloud_saves::snapshot_download_urls),
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
        .route(
            "/profile/playtime",
            get(playtime::heatmap).post(playtime::report),
        )
        .route(
            "/profile/playtime/{user_id}",
            get(playtime::user_heatmap),
        )
        .route("/profile/members/{user_id}", get(members::lookup))
        /* Achievement souvenirs. The per-souvenir routes sit under /profile,
           the profile-facing ones under /users/{id} — the same split upstream
           uses, and the launcher builds both. */
        .route(
            "/profile/souvenirs-visibility",
            patch(souvenirs::set_account_visibility),
        )
        .route(
            "/profile/souvenirs/{id}/visibility",
            patch(souvenirs::set_visibility),
        )
        .route("/profile/souvenirs/{id}", delete(souvenirs::delete))
        .route("/users/{user_id}/souvenirs", get(souvenirs::list_for_user))
        .route(
            "/users/{user_id}/souvenirs/{souvenir_id}/like",
            post(souvenirs::like),
        )
        .route(
            "/users/{user_id}/souvenirs/{souvenir_id}/report",
            post(souvenirs::report),
        )
        /* Souvenir thumbnails for the achievement list. Named as upstream
           names it, so the launcher reads the images off the same endpoint it
           already asks for a profile's achievements. */
        .route(
            "/users/{user_id}/games/achievements",
            get(souvenirs::user_game_achievements),
        )
        .route("/presigned-urls/{type}", post(images::presign))
        .route("/profile/stats/{user_id}", get(achievements::user_stats))
        .route("/profile/achievements/{user_id}", get(achievements::recent))
        .route("/profile/banners/{user_id}", get(images::get_banner))
        .route("/profile/banner", delete(images::delete_banner))
        .route("/images/{*path}", get(images::serve))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));

    Router::new()
        .route("/health", get(health))
        .route("/capabilities", get(capabilities))
        .route("/metrics", get(metrics::render))
        .merge(api_routes)
        .merge(storage_routes)
        .merge(assets::router())
        .merge(admin::router())
        .merge(portal::router())
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

/// What this server can actually do, so the launcher never enables a feature
/// whose endpoints aren't here.
///
/// Upstream keeps adding subscription-gated features that the launcher routes
/// straight to whichever server is configured; without this the launcher can
/// only find out by getting a 404 mid-sync. `features` is the contract — the
/// launcher matches on those strings, and `version` (kept in step with the
/// launcher release this server targets) is only for display and support.
///
/// Unauthenticated on purpose: the launcher needs it before it has decided
/// whether the server is usable at all, and it discloses nothing user-specific.
async fn capabilities() -> Json<serde_json::Value> {
    Json(json!({
        "name": "hydra-server",
        "version": env!("CARGO_PKG_VERSION"),
        "features": [
            "cloud-saves-v2",
            "user-portal",
            "cloud-saves-legacy",
            "emulation-saves",
            "custom-artwork",
            "achievements",
            "playtime",
            "download-sources",
            "banners",
            "artifact-shares",
            "souvenirs",
        ],
    }))
}
