//! Brute-force protection for the two password forms.
//!
//! The admin panel is guarded by a single shared password and the portal
//! forwards credentials to the official Hydra API — both are worth exactly
//! one thing to an attacker with unlimited guesses. This keeps a small
//! per-address failure counter in memory (there is no cluster to share it
//! with) and locks an address out after too many misses.

use crate::error::ApiError;
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// Failures older than this stop counting, so an occasional typo never
/// accumulates into a lockout.
const WINDOW_MINUTES: i64 = 15;

#[derive(Default)]
pub struct Attempts {
    failures: u32,
    first_failure_at: Option<DateTime<Utc>>,
    locked_until: Option<DateTime<Utc>>,
}

/// Keyed by "<scope>:<address>" so a portal lockout doesn't lock the admin
/// panel and vice versa.
pub type Guard = HashMap<String, Attempts>;

fn key(scope: &str, ip: &str) -> String {
    format!("{scope}:{ip}")
}

/// Refuses the attempt when the address is locked out.
pub async fn check(state: &AppState, scope: &str, ip: &str) -> Result<(), ApiError> {
    let guard = state.login_guard.read().await;

    let Some(attempts) = guard.get(&key(scope, ip)) else {
        return Ok(());
    };

    let Some(until) = attempts.locked_until else {
        return Ok(());
    };

    let remaining = (until - Utc::now()).num_seconds();
    if remaining <= 0 {
        return Ok(());
    }

    Err(ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        format!(
            "too many failed attempts — try again in {} minute(s)",
            remaining.div_euclid(60) + 1
        ),
    ))
}

/// Records a miss. Returns the lockout deadline when this was the last straw,
/// so the caller can log it.
pub async fn record_failure(
    state: &AppState,
    scope: &str,
    ip: &str,
) -> (u32, Option<DateTime<Utc>>) {
    let now = Utc::now();
    let max_attempts = state.config.login_max_attempts.max(1);
    let lockout = Duration::minutes(state.config.login_lockout_minutes.max(1));

    let mut guard = state.login_guard.write().await;

    /* Opportunistic cleanup: this map is only touched on failures, so it can
    be tidied here instead of on a timer. */
    guard.retain(|_, attempts| {
        let recent = attempts
            .first_failure_at
            .is_some_and(|at| now - at < Duration::minutes(WINDOW_MINUTES));
        let locked = attempts.locked_until.is_some_and(|until| until > now);
        recent || locked
    });

    let entry = guard.entry(key(scope, ip)).or_default();

    let stale = entry
        .first_failure_at
        .is_none_or(|at| now - at >= Duration::minutes(WINDOW_MINUTES));
    if stale {
        entry.failures = 0;
        entry.first_failure_at = Some(now);
    }

    entry.failures += 1;

    if entry.failures >= max_attempts {
        entry.locked_until = Some(now + lockout);
        return (entry.failures, entry.locked_until);
    }

    (entry.failures, None)
}

/// A success clears the slate for that address.
pub async fn record_success(state: &AppState, scope: &str, ip: &str) {
    state.login_guard.write().await.remove(&key(scope, ip));
}
