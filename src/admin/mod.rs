//! The admin panel: a self-contained operations console for a self-hosted
//! Hydra cloud server.
//!
//! Everything an operator needs lives here — who is using the server, what
//! they stored, which games it holds, whether the bytes on disk still match
//! the database, and the handful of knobs that can be turned without a
//! restart.
//!
//! # Layout
//!
//! One module per area of the panel, each owning its own routes so a new
//! feature means a new module and one `merge` below rather than edits spread
//! across a monolith:
//!
//! | module | area |
//! | --- | --- |
//! | [`session`] | login, logout, the cookie session every other route requires |
//! | [`events`] | the searchable history of everything the server recorded |
//! | [`assets`] | the panel's own HTML/CSS/JS, embedded in the binary |
//! | [`overview`] | dashboard totals, alerts, activity feed, trends |
//! | [`users`] | the user directory, per-user detail, blocking and data purges |
//! | [`saves`] | every stored save across users: V2 snapshots, legacy backups, emulation saves |
//! | [`games`] | the same data pivoted by game rather than by user |
//! | [`storage`] | what occupies disk, and whether disk and database agree |
//! | [`maintenance`] | one-shot operations: sweeps, garbage collection, metadata refresh, export |
//! | [`schedule`] | the timetable those same operations run on unattended, and each one's log |
//! | [`backups`] | database backups: take one, download it, prune the rest |
//! | [`webhooks`] | outbound notifications for anything in the event log |
//! | [`settings`] | the runtime settings the panel may change |
//!
//! The front end mirrors that split (`static/admin/js/views/*`), so a feature
//! is usually one module here plus one view there.

use crate::state::AppState;
use axum::Router;
use sqlx::Row;

mod assets;
mod backups;
mod events;
mod games;
mod maintenance;
mod overview;
mod saves;
mod schedule;
mod session;
mod settings;
mod storage;
mod users;
mod webhooks;

pub use session::AdminSession;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(assets::router())
        .merge(session::router())
        .merge(overview::router())
        .merge(events::router())
        .merge(users::router())
        .merge(saves::router())
        .merge(games::router())
        .merge(storage::router())
        .merge(maintenance::router())
        .merge(schedule::router())
        .merge(backups::router())
        .merge(webhooks::router())
        .merge(settings::router())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Public URL of a banner stored on this server (`banner_key`), if any.
pub(crate) fn banner_url(state: &AppState, banner_key: Option<String>) -> Option<String> {
    banner_key.map(|key| {
        format!(
            "{}/{}",
            state.config.public_url,
            key.trim_start_matches('/')
        )
    })
}

/// The identity block every list that mentions a user repeats.
pub(crate) fn user_ref(row: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.get::<Option<String>, _>("user_id"),
        "displayName": row.get::<Option<String>, _>("display_name"),
        "username": row.get::<Option<String>, _>("username"),
        "profileImageUrl": row.get::<Option<String>, _>("profile_image_url"),
    })
}

/// The game block every list that mentions a game repeats. Names and covers
/// come from the metadata cache and stay null until something resolves them;
/// the panel falls back to `shop/objectId`.
pub(crate) fn game_ref(row: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    serde_json::json!({
        "shop": row.get::<Option<String>, _>("shop"),
        "objectId": row.get::<Option<String>, _>("object_id"),
        "name": row.get::<Option<String>, _>("game_name"),
        "coverUrl": row.get::<Option<String>, _>("game_cover_url"),
    })
}

/// Page/size for a listing endpoint, clamped so a hand-written query string
/// can't ask for the whole database.
///
/// Built from a listing's own `page`/`perPage` fields rather than flattened
/// into its query struct: `Query` deserializes from a flat string map, where
/// `#[serde(flatten)]` would hand `page` to the wrong deserializer.
#[derive(Default)]
pub(crate) struct Paging {
    page: Option<i64>,
    per_page: Option<i64>,
}

impl Paging {
    pub fn new(page: Option<i64>, per_page: Option<i64>) -> Self {
        Self { page, per_page }
    }

    pub fn page(&self) -> i64 {
        self.page.unwrap_or(1).max(1)
    }

    pub fn per_page(&self) -> i64 {
        self.per_page.unwrap_or(25).clamp(1, 200)
    }

    pub fn offset(&self) -> i64 {
        (self.page() - 1) * self.per_page()
    }

    /// The envelope every paginated listing returns.
    pub fn envelope(&self, rows: Vec<serde_json::Value>, total: i64) -> serde_json::Value {
        serde_json::json!({
            "rows": rows,
            "total": total,
            "page": self.page(),
            "perPage": self.per_page(),
            "pageCount": (total as f64 / self.per_page() as f64).ceil() as i64,
        })
    }
}

/// Resolves a client-supplied sort key against a whitelist of SQL fragments.
///
/// The key never reaches SQL — only the matching fragment does — so sorting
/// stays injection-proof no matter what the query string contains.
pub(crate) fn order_by(
    columns: &[(&str, &str)],
    key: Option<&str>,
    direction: Option<&str>,
    fallback: &str,
) -> String {
    let column = key
        .and_then(|key| {
            columns
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, expr)| *expr)
        })
        .unwrap_or(fallback);

    let direction = match direction {
        Some("asc") => "ASC",
        _ => "DESC",
    };

    format!("{column} {direction}")
}

/// `LIKE` pattern for a free-text filter, with the wildcards the user didn't
/// type. Returns `None` for a blank search so callers can skip the clause.
pub(crate) fn like_pattern(query: Option<&str>) -> Option<String> {
    let query = query?.trim();
    if query.is_empty() {
        return None;
    }
    /* Escaping keeps a literal % or _ from turning into a wildcard; the
    clauses that use this pattern declare ESCAPE '\'. */
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    Some(format!("%{escaped}%"))
}

/// These three helpers are the only places where a client-supplied value gets
/// anywhere near SQL, so their guarantees are pinned rather than assumed.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_keys_never_reach_sql() {
        let columns = &[("size", "x.size_bytes"), ("name", "g.name")];

        assert_eq!(
            order_by(columns, Some("size"), Some("asc"), "x.at"),
            "x.size_bytes ASC"
        );
        assert_eq!(order_by(columns, Some("name"), None, "x.at"), "g.name DESC");

        /* Anything not on the list falls back to the default column, and the
        direction only ever renders as one of two literals. */
        assert_eq!(
            order_by(columns, Some("x.at; DROP TABLE users"), Some("asc"), "x.at"),
            "x.at ASC"
        );
        assert_eq!(
            order_by(columns, None, Some("' OR 1=1"), "x.at"),
            "x.at DESC"
        );
    }

    #[test]
    fn search_patterns_escape_their_own_wildcards() {
        assert_eq!(like_pattern(Some("nova")).unwrap(), "%nova%");
        /* A user searching for "100%" wants that literal, not "anything". */
        assert_eq!(like_pattern(Some("100%")).unwrap(), "%100\\%%");
        assert_eq!(like_pattern(Some("a_b")).unwrap(), "%a\\_b%");
        assert_eq!(
            like_pattern(Some("back\\slash")).unwrap(),
            "%back\\\\slash%"
        );

        assert!(like_pattern(Some("   ")).is_none());
        assert!(like_pattern(None).is_none());
    }

    #[test]
    fn paging_clamps_what_a_query_string_can_ask_for() {
        let paging = Paging::new(Some(3), Some(50));
        assert_eq!(
            (paging.page(), paging.per_page(), paging.offset()),
            (3, 50, 100)
        );

        /* Page 0 and negative pages would produce a negative OFFSET. */
        assert_eq!(Paging::new(Some(0), None).page(), 1);
        assert_eq!(Paging::new(Some(-5), None).offset(), 0);

        assert_eq!(Paging::new(None, Some(100_000)).per_page(), 200);
        assert_eq!(Paging::new(None, Some(0)).per_page(), 1);
        assert_eq!(Paging::new(None, None).per_page(), 25);
    }

    #[test]
    fn envelope_reports_the_page_count_a_pager_needs() {
        let paging = Paging::new(Some(1), Some(25));
        let envelope = paging.envelope(vec![], 51);

        assert_eq!(envelope["total"], 51);
        assert_eq!(envelope["pageCount"], 3);
        assert_eq!(paging.envelope(vec![], 0)["pageCount"], 0);
    }
}
