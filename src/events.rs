//! The server's event log.
//!
//! Three consumers, one write path:
//!
//! * the **Events screen** in the admin panel — the searchable history of
//!   everything the server has done, including things that no longer exist;
//! * the **audit trail** — every operator action, recorded with what it
//!   affected and how much it freed, and kept when the account it refers to
//!   is deleted;
//! * **webhooks** — each recorded event is offered to whatever the operator
//!   wired up.
//!
//! Recording is deliberately infallible from the caller's point of view: a log
//! write must never turn a working upload into a failed one, so errors here
//! are traced and swallowed.

use crate::state::AppState;
use chrono::Utc;
use serde_json::Value;
use sqlx::Row;

/// Where an event came from. Drives the filter chips in the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A launcher storing or fetching something.
    Sync,
    /// An operator acting through the admin panel.
    Admin,
    /// Sign-ins, lockouts and rejected access.
    Auth,
    /// Background work: startup, sweeps, garbage collection, backups.
    System,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Category::Sync => "sync",
            Category::Admin => "admin",
            Category::Auth => "auth",
            Category::System => "system",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            _ => Severity::Info,
        }
    }
}

/// The longest title an event keeps. The history lists one event per line, so a
/// title is a label: anything that needs a sentence goes in the description.
const MAX_SUMMARY_CHARS: usize = 140;

/// One thing that happened. Built fluently at the call site so recording an
/// event stays a single readable statement.
pub struct Event {
    pub kind: &'static str,
    pub category: Category,
    pub severity: Severity,
    pub actor: Option<String>,
    pub user_id: Option<String>,
    pub shop: Option<String>,
    pub object_id: Option<String>,
    /// The title: short enough for one line of the history.
    pub summary: String,
    /// Longer text for whoever opens the event to find out more. Kept with the
    /// detail, under `description`.
    pub description: Option<String>,
    pub detail: Option<Value>,
    pub ip: Option<String>,
    pub size_bytes: Option<i64>,
}

impl Event {
    fn new(kind: &'static str, category: Category, summary: impl Into<String>) -> Self {
        Self {
            kind,
            category,
            severity: Severity::Info,
            actor: None,
            user_id: None,
            shop: None,
            object_id: None,
            summary: summary.into(),
            description: None,
            detail: None,
            ip: None,
            size_bytes: None,
        }
    }

    /// A launcher did something. The user is both actor and subject.
    pub fn sync(kind: &'static str, user_id: &str, summary: impl Into<String>) -> Self {
        Self::new(kind, Category::Sync, summary)
            .actor(format!("user:{user_id}"))
            .about(user_id)
    }

    /// An operator did something through the panel.
    pub fn admin(kind: &'static str, summary: impl Into<String>) -> Self {
        Self::new(kind, Category::Admin, summary).actor("admin")
    }

    pub fn auth(kind: &'static str, summary: impl Into<String>) -> Self {
        Self::new(kind, Category::Auth, summary)
    }

    pub fn system(kind: &'static str, summary: impl Into<String>) -> Self {
        Self::new(kind, Category::System, summary).actor("system")
    }

    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn about(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn game(mut self, shop: impl Into<String>, object_id: impl Into<String>) -> Self {
        self.shop = Some(shop.into());
        self.object_id = Some(object_id.into());
        self
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// The sentence or two behind the title: what happened, why it matters,
    /// what to check. Shown in the event's detail view, not in the list.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn size(mut self, bytes: i64) -> Self {
        self.size_bytes = Some(bytes);
        self
    }

    pub fn ip(mut self, ip: Option<String>) -> Self {
        self.ip = ip;
        self
    }

    pub fn warning(mut self) -> Self {
        self.severity = Severity::Warning;
        self
    }

    pub fn critical(mut self) -> Self {
        self.severity = Severity::Critical;
        self
    }
}

impl Event {
    /// Puts the event in the shape it is stored and sent in: a title of one
    /// line, and the description beside the rest of the detail.
    ///
    /// A title that runs past the limit is cut and its full text becomes the
    /// description, unless the caller wrote one, so no event can fill the list
    /// with a sentence however it was written.
    fn normalize(&mut self) {
        if self.summary.chars().count() > MAX_SUMMARY_CHARS {
            let full = std::mem::take(&mut self.summary);
            self.summary =
                full.chars().take(MAX_SUMMARY_CHARS - 1).collect::<String>() + "\u{2026}";
            self.description.get_or_insert(full);
        }

        let Some(description) = self.description.take() else {
            return;
        };

        match self.detail.take() {
            Some(Value::Object(mut detail)) => {
                detail.insert("description".to_string(), Value::String(description));
                self.detail = Some(Value::Object(detail));
            }
            /* Not what events carry, but not a reason to lose either. */
            Some(other) => {
                self.detail = Some(serde_json::json!({
                    "description": description,
                    "detail": other,
                }));
            }
            None => {
                self.detail = Some(serde_json::json!({ "description": description }));
            }
        }
    }
}

/// Writes the event and hands it to the webhook dispatcher.
///
/// Never fails the caller: a request that did its job must not report an
/// error because the log did not.
pub async fn record(state: &AppState, mut event: Event) {
    event.normalize();
    let at = Utc::now().to_rfc3339();

    let detail = event.detail.as_ref().map(|detail| detail.to_string());

    let result = sqlx::query(
        "INSERT INTO events
           (at, kind, category, severity, actor, user_id, shop, object_id,
            summary, detail, ip, size_bytes)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&at)
    .bind(event.kind)
    .bind(event.category.as_str())
    .bind(event.severity.as_str())
    .bind(&event.actor)
    .bind(&event.user_id)
    .bind(&event.shop)
    .bind(&event.object_id)
    .bind(&event.summary)
    .bind(&detail)
    .bind(&event.ip)
    .bind(event.size_bytes)
    .execute(&state.pool)
    .await;

    if let Err(err) = result {
        tracing::warn!("failed to record event {}: {err}", event.kind);
    }

    crate::webhooks::dispatch(state, &event, &at).await;
}

/// Drops events older than the retention window. Called from maintenance and
/// from the daily background job.
pub async fn prune(state: &AppState, keep_days: i64) -> Result<u64, sqlx::Error> {
    let cutoff = (Utc::now() - chrono::Duration::days(keep_days.max(1))).to_rfc3339();

    let result = sqlx::query("DELETE FROM events WHERE at < ?")
        .bind(&cutoff)
        .execute(&state.pool)
        .await?;

    Ok(result.rows_affected())
}

/// Rows shaped for the panel, with the user and game joined in.
pub fn row_json(row: &sqlx::sqlite::SqliteRow) -> Value {
    serde_json::json!({
        "id": row.get::<i64, _>("id"),
        "at": row.get::<String, _>("at"),
        "kind": row.get::<String, _>("kind"),
        "category": row.get::<String, _>("category"),
        "severity": row.get::<String, _>("severity"),
        "actor": row.get::<Option<String>, _>("actor"),
        "summary": row.get::<String, _>("summary"),
        "detail": row
            .get::<Option<String>, _>("detail")
            .and_then(|detail| serde_json::from_str::<Value>(&detail).ok()),
        "ip": row.get::<Option<String>, _>("ip"),
        "sizeBytes": row.get::<Option<i64>, _>("size_bytes"),
        "user": {
            "id": row.get::<Option<String>, _>("user_id"),
            "displayName": row.get::<Option<String>, _>("display_name"),
            "username": row.get::<Option<String>, _>("username"),
            "profileImageUrl": row.get::<Option<String>, _>("profile_image_url"),
        },
        "game": {
            "shop": row.get::<Option<String>, _>("shop"),
            "objectId": row.get::<Option<String>, _>("object_id"),
            "name": row.get::<Option<String>, _>("game_name"),
            "coverUrl": row.get::<Option<String>, _>("game_cover_url"),
        },
    })
}

/// The join every event listing needs: the subject user and the game, both
/// optional because an event may refer to neither.
pub const EVENT_JOINS: &str = "
    FROM events e
    LEFT JOIN users u ON u.id = e.user_id
    LEFT JOIN game_metadata g ON g.shop = e.shop AND g.object_id = e.object_id";

pub const EVENT_COLUMNS: &str = "
    e.*, u.display_name, u.username, u.profile_image_url,
    g.name AS game_name, g.cover_url AS game_cover_url";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;
    use serde_json::json;

    async fn stored(server: &TestServer) -> (String, Value) {
        let (summary, detail): (String, Option<String>) =
            sqlx::query_as("SELECT summary, detail FROM events ORDER BY id DESC LIMIT 1")
                .fetch_one(&server.state.pool)
                .await
                .expect("an event");

        (
            summary,
            serde_json::from_str(&detail.expect("detail")).expect("json"),
        )
    }

    #[tokio::test]
    async fn a_description_is_kept_with_the_detail_and_the_title_is_left_alone() {
        let server = TestServer::start().await;

        record(
            &server.state,
            Event::sync("test.described", "alice", "Short title")
                .description("The longer story, for whoever opens the event.")
                .detail(json!({ "stage": "upload" })),
        )
        .await;

        let (summary, detail) = stored(&server).await;
        assert_eq!(summary, "Short title");
        assert_eq!(
            detail["description"],
            "The longer story, for whoever opens the event."
        );
        assert_eq!(detail["stage"], "upload", "what was there stays");
    }

    #[tokio::test]
    async fn a_description_alone_still_has_somewhere_to_live() {
        let server = TestServer::start().await;

        record(
            &server.state,
            Event::sync("test.described", "alice", "Short title").description("More."),
        )
        .await;

        assert_eq!(stored(&server).await.1, json!({ "description": "More." }));
    }

    #[tokio::test]
    async fn a_title_that_runs_on_is_cut_and_its_text_kept_as_the_description() {
        let server = TestServer::start().await;
        let long = format!("{} end", "word ".repeat(80));

        record(
            &server.state,
            Event::sync("test.long", "alice", long.clone()).detail(json!({ "kept": true })),
        )
        .await;

        let (summary, detail) = stored(&server).await;
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(summary.ends_with('\u{2026}'), "{summary}");
        assert_eq!(detail["description"], long.as_str(), "nothing is lost");
        assert_eq!(detail["kept"], true);
    }

    #[tokio::test]
    async fn a_description_the_caller_wrote_is_not_replaced_by_a_cut_title() {
        let server = TestServer::start().await;

        record(
            &server.state,
            Event::sync("test.long", "alice", "x".repeat(500)).description("Written by hand."),
        )
        .await;

        let (summary, detail) = stored(&server).await;
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        assert_eq!(detail["description"], "Written by hand.");
    }

    #[tokio::test]
    async fn a_title_that_fits_is_stored_as_it_was_written() {
        let server = TestServer::start().await;
        let exactly = "y".repeat(MAX_SUMMARY_CHARS);

        record(
            &server.state,
            Event::sync("test.fits", "alice", exactly.clone()),
        )
        .await;

        let (summary, detail): (String, Option<String>) =
            sqlx::query_as("SELECT summary, detail FROM events ORDER BY id DESC LIMIT 1")
                .fetch_one(&server.state.pool)
                .await
                .expect("an event");
        assert_eq!(summary, exactly);
        assert_eq!(detail, None);
    }

    #[test]
    fn a_detail_that_is_not_an_object_is_wrapped_rather_than_lost() {
        let mut event = Event::sync("test.odd", "alice", "Title")
            .description("Words.")
            .detail(json!([1, 2]));

        event.normalize();

        assert_eq!(
            event.detail,
            Some(json!({ "description": "Words.", "detail": [1, 2] }))
        );
    }
}
