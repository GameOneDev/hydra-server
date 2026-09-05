//! The front end, embedded in the binary and served from one place.
//!
//! The admin panel and the user portal are separate apps that share a design
//! system and a component library, so everything lives under a single
//! `/assets/…` prefix and modules import each other by absolute URL. That
//! keeps the deployment story unchanged — one binary, no asset directory to
//! ship or misconfigure — while letting either app grow without copying the
//! other's building blocks.
//!
//! To add a file: drop it under `static/` and add one row to [`ASSETS`].

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

pub fn router() -> Router<AppState> {
    Router::new().route("/assets/{*path}", get(asset))
}

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("../static/", $path)))),*]
    };
}

const ASSETS: &[(&str, &str)] = assets![
    // Design system and components, used by both apps.
    "shared/app.css",
    "shared/js/api.js",
    "shared/js/dom.js",
    "shared/js/format.js",
    "shared/js/router.js",
    "shared/js/components/ui.js",
    "shared/js/components/table.js",
    "shared/js/components/charts.js",
    // Admin panel.
    "admin/js/main.js",
    "admin/js/store.js",
    "admin/js/components/shell.js",
    "admin/js/components/palette.js",
    "admin/js/views/login.js",
    "admin/js/views/overview.js",
    "admin/js/views/events.js",
    "admin/js/views/users.js",
    "admin/js/views/user.js",
    "admin/js/views/saves.js",
    "admin/js/views/games.js",
    "admin/js/views/storage.js",
    "admin/js/views/maintenance.js",
    "admin/js/views/schedule.js",
    "admin/js/views/webhooks.js",
    "admin/js/views/settings.js",
    // User portal.
    "portal/js/main.js",
    "portal/js/views/login.js",
    "portal/js/views/home.js",
    "portal/js/views/saves.js",
    "portal/js/views/library.js",
];

async fn asset(Path(path): Path<String>) -> ApiResult<Response> {
    let (_, body) = ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .ok_or_else(|| ApiError::not_found("asset not found"))?;

    let content_type = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "text/plain; charset=utf-8",
    };

    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            /* Revalidate every load: the front end ships inside the binary,
            so a server upgrade must never be shadowed by a cached module. */
            (header::CACHE_CONTROL, "no-cache"),
        ],
        *body,
    )
        .into_response())
}
