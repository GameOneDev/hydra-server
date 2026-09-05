//! Who is around.
//!
//! The launcher has no notion of signing out, and no session to end — it just
//! stops calling. So "online" is defined here as *calling at all*, and coming
//! online is the first call after a quiet spell long enough to count as away
//! (`HYDRA_PRESENCE_IDLE_MINUTES`).
//!
//! This runs on the authenticated path of every request, so the common case —
//! a client that is already known to be here — must touch nothing but a read
//! lock.

use crate::events::Event;
use crate::state::{AppState, AuthenticatedUser};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

/// How stale the in-memory timestamp is allowed to get before a request pays
/// for the write lock. Far below any sensible idle window, so it cannot turn
/// a present user into an absent one.
const REFRESH_SECONDS: i64 = 60;

/// Records that `user` is here, and logs an event when that is news.
pub async fn touch(state: &AppState, user: &AuthenticatedUser, ip: Option<String>) {
    let idle_minutes = state.config.presence_idle_minutes;
    if idle_minutes <= 0 {
        return;
    }

    let now = Utc::now();

    /* Hot path: seen recently, nothing to decide and nothing to write. */
    {
        let seen = state.presence.read().await;
        if let Some(last) = seen.get(&user.id) {
            if (now - *last).num_seconds() < REFRESH_SECONDS {
                return;
            }
        }
    }

    let idle = Duration::minutes(idle_minutes);

    let previous = {
        let mut seen = state.presence.write().await;
        /* Opportunistic cleanup, on the same slow path as everything else
        here: entries past the idle window are re-derived from the database
        anyway, so keeping them buys nothing. */
        seen.retain(|_, last| now - *last < idle);
        seen.insert(user.id.clone(), now)
    };

    let previous = match previous {
        Some(at) => Some(at),
        /* Nothing in memory: either this process just started or the entry
        aged out. The database remembers what the process doesn't, which is
        what keeps a restart from announcing everybody as newly arrived. */
        None => last_seen(state, &user.id).await,
    };

    if !is_return(previous, now, idle) {
        return;
    }

    let away = previous.map(|at| now - at);

    crate::events::record(
        state,
        Event::auth("user.online", format!("{} came online", user.display_name))
            .actor(format!("user:{}", user.id))
            .about(&user.id)
            .ip(ip)
            .detail(json!({
                "minutesAway": away.map(|gap| gap.num_minutes()),
                "lastSeenAt": previous.map(|at| at.to_rfc3339()),
            })),
    )
    .await;
}

/// Whether this sighting counts as coming online rather than carrying on.
fn is_return(previous: Option<DateTime<Utc>>, now: DateTime<Utc>, idle: Duration) -> bool {
    match previous {
        Some(at) => now - at >= idle,
        /* No record at all — a user row without a timestamp. Treat an unknown
        past as an absent one; the alternative is never reporting them. */
        None => true,
    }
}

async fn last_seen(state: &AppState, user_id: &str) -> Option<DateTime<Utc>> {
    let stored: Option<(Option<String>,)> =
        sqlx::query_as("SELECT last_seen_at FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();

    stored
        .and_then(|(at,)| at)
        .and_then(|at| DateTime::parse_from_rfc3339(&at).ok())
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_absence_counts_as_coming_online() {
        let now = Utc::now();
        let idle = Duration::minutes(15);

        /* Mid-session: a client polls constantly, and none of that is news. */
        assert!(!is_return(Some(now - Duration::seconds(90)), now, idle));
        assert!(!is_return(Some(now - Duration::minutes(14)), now, idle));

        assert!(is_return(Some(now - Duration::minutes(15)), now, idle));
        assert!(is_return(Some(now - Duration::days(3)), now, idle));
        assert!(is_return(None, now, idle));
    }
}
