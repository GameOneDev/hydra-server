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
    pub summary: String,
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

/// Writes the event and hands it to the webhook dispatcher.
///
/// Never fails the caller: a request that did its job must not report an
/// error because the log did not.
pub async fn record(state: &AppState, event: Event) {
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
