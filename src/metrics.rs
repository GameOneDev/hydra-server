//! Prometheus metrics at `/metrics`.
//!
//! Counters live in memory and reset on restart (that is what `_total` means
//! to Prometheus); gauges are read from the database at scrape time, which is
//! cheap on a database this size and always tells the truth.

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-lifetime counters, incremented on the request path.
#[derive(Default)]
pub struct Counters {
    pub requests: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
    pub bytes_uploaded: AtomicU64,
    pub bytes_downloaded: AtomicU64,
    pub login_failures: AtomicU64,
}

impl Counters {
    pub fn observe_status(&self, status: u16) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let bucket = match status {
            200..=299 => &self.responses_2xx,
            400..=499 => &self.responses_4xx,
            500..=599 => &self.responses_5xx,
            _ => return,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_uploaded(&self, bytes: u64) {
        self.bytes_uploaded.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_downloaded(&self, bytes: u64) {
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }
}

fn metric(out: &mut String, name: &str, help: &str, kind: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

fn labelled(out: &mut String, name: &str, label: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "{name}{{{label}}} {value}");
}

async fn scalar(state: &AppState, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&state.pool)
        .await
        .unwrap_or_default()
}

/// GET /metrics — Prometheus text exposition.
///
/// Guarded by a bearer token when `HYDRA_METRICS_TOKEN` is set. It exposes no
/// personal data (counts and byte totals only), so leaving it open on a LAN
/// is a reasonable default.
pub async fn render(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if !state.config.metrics_enabled {
        return Err(ApiError::not_found("metrics are disabled"));
    }

    if !state.config.metrics_token.is_empty() {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();

        if presented != state.config.metrics_token {
            return Err(ApiError::unauthorized("invalid metrics token"));
        }
    }

    let counters = &state.metrics;
    let mut out = String::with_capacity(4096);

    metric(
        &mut out,
        "hydra_up",
        "Always 1; scrape target liveness.",
        "gauge",
        1,
    );
    metric(
        &mut out,
        "hydra_uptime_seconds",
        "Seconds since this process started.",
        "gauge",
        (chrono::Utc::now() - state.started_at).num_seconds(),
    );

    // --- request counters ---------------------------------------------------
    metric(
        &mut out,
        "hydra_http_requests_total",
        "HTTP requests served since start.",
        "counter",
        counters.requests.load(Ordering::Relaxed),
    );
    let _ = writeln!(
        out,
        "# HELP hydra_http_responses_total HTTP responses by status class.\n# TYPE hydra_http_responses_total counter"
    );
    labelled(&mut out, "hydra_http_responses_total", "class=\"2xx\"", counters.responses_2xx.load(Ordering::Relaxed));
    labelled(&mut out, "hydra_http_responses_total", "class=\"4xx\"", counters.responses_4xx.load(Ordering::Relaxed));
    labelled(&mut out, "hydra_http_responses_total", "class=\"5xx\"", counters.responses_5xx.load(Ordering::Relaxed));

    metric(
        &mut out,
        "hydra_storage_bytes_uploaded_total",
        "Bytes accepted by the storage endpoint since start.",
        "counter",
        counters.bytes_uploaded.load(Ordering::Relaxed),
    );
    metric(
        &mut out,
        "hydra_storage_bytes_downloaded_total",
        "Bytes served by the storage endpoint since start.",
        "counter",
        counters.bytes_downloaded.load(Ordering::Relaxed),
    );
    metric(
        &mut out,
        "hydra_login_failures_total",
        "Failed admin and portal sign-in attempts since start.",
        "counter",
        counters.login_failures.load(Ordering::Relaxed),
    );

    // --- database gauges ----------------------------------------------------
    metric(
        &mut out,
        "hydra_users",
        "Registered users.",
        "gauge",
        scalar(&state, "SELECT COUNT(*) FROM users").await,
    );
    metric(
        &mut out,
        "hydra_users_blocked",
        "Users blocked from syncing.",
        "gauge",
        scalar(&state, "SELECT COUNT(*) FROM users WHERE is_blocked = 1").await,
    );

    let _ = writeln!(
        out,
        "# HELP hydra_stored_bytes Stored bytes by kind.\n# TYPE hydra_stored_bytes gauge"
    );
    for (label, sql) in [
        ("cloud_saves", "SELECT COALESCE(SUM(size_in_bytes), 0) FROM cloud_save_blobs"),
        ("backups", "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM artifacts"),
        ("emulation_saves", "SELECT COALESCE(SUM(artifact_length_in_bytes), 0) FROM emulation_saves"),
        ("artwork", "SELECT COALESCE(SUM(size_in_bytes), 0) FROM game_artwork"),
        ("souvenirs", "SELECT COALESCE(SUM(size_in_bytes), 0) FROM souvenirs"),
    ] {
        labelled(
            &mut out,
            "hydra_stored_bytes",
            &format!("kind=\"{label}\""),
            scalar(&state, sql).await,
        );
    }

    let _ = writeln!(
        out,
        "# HELP hydra_saves Stored saves by kind.\n# TYPE hydra_saves gauge"
    );
    for (label, sql) in [
        ("cloud_saves", "SELECT COUNT(*) FROM cloud_save_snapshots WHERE status = 'committed'"),
        ("backups", "SELECT COUNT(*) FROM artifacts"),
        ("emulation_saves", "SELECT COUNT(*) FROM emulation_saves"),
    ] {
        labelled(
            &mut out,
            "hydra_saves",
            &format!("kind=\"{label}\""),
            scalar(&state, sql).await,
        );
    }

    metric(
        &mut out,
        "hydra_cloud_save_uploads_pending",
        "Snapshots prepared but never committed.",
        "gauge",
        scalar(
            &state,
            "SELECT COUNT(*) FROM cloud_save_snapshots WHERE status = 'pending'",
        )
        .await,
    );
    metric(
        &mut out,
        "hydra_cloud_save_blobs",
        "Distinct content-addressed blobs held.",
        "gauge",
        scalar(&state, "SELECT COUNT(*) FROM cloud_save_blobs").await,
    );
    metric(
        &mut out,
        "hydra_playtime_seconds",
        "Playtime reported by every launcher, all time.",
        "gauge",
        scalar(&state, "SELECT COALESCE(SUM(seconds), 0) FROM playtime_daily").await,
    );
    metric(
        &mut out,
        "hydra_webhooks_failing",
        "Webhooks with a failed last delivery.",
        "gauge",
        scalar(
            &state,
            "SELECT COUNT(*) FROM webhooks WHERE last_status = 'failed'",
        )
        .await,
    );

    /* Events in the last hour, by category: the cheapest signal of "is the
       server actually being used" for a dashboard. */
    let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT category, COUNT(*) FROM events WHERE at >= ? GROUP BY category",
    )
    .bind(&since)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let _ = writeln!(
        out,
        "# HELP hydra_events_last_hour Events recorded in the last hour, by category.\n# TYPE hydra_events_last_hour gauge"
    );
    for (category, count) in rows {
        labelled(
            &mut out,
            "hydra_events_last_hour",
            &format!("category=\"{category}\""),
            count,
        );
    }

    let mut database_bytes = 0u64;
    let db_path = state.config.database_path();
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db_path.display()));
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            database_bytes += meta.len();
        }
    }
    metric(
        &mut out,
        "hydra_database_bytes",
        "Size of the SQLite database including its WAL.",
        "gauge",
        database_bytes,
    );

    if let Some(free) = crate::backup::free_disk_bytes(&state.config.data_dir) {
        metric(
            &mut out,
            "hydra_disk_free_bytes",
            "Free space on the volume holding the data directory.",
            "gauge",
            free,
        );
    }

    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        out,
    )
        .into_response())
}
