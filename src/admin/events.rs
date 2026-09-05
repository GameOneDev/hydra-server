//! The history screen: everything the server has recorded, searchable.

use super::{AdminSession, Paging};
use crate::error::ApiResult;
use crate::events::{EVENT_COLUMNS, EVENT_JOINS};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/events", get(list))
        .route("/admin/api/events/kinds", get(kinds))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    /// Inclusive calendar days, as `YYYY-MM-DD`.
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    per_page: Option<i64>,
}

/// GET /admin/api/events — the log, filtered.
async fn list(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let paging = Paging::new(query.page, query.per_page);

    let mut filters: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    let mut add = |value: String, clause: &dyn Fn(usize) -> String| {
        binds.push(value);
        filters.push(clause(binds.len()));
    };

    if let Some(category) = query.category.as_deref().filter(|value| !value.is_empty()) {
        add(category.to_string(), &|i| format!("e.category = ?{i}"));
    }
    if let Some(severity) = query.severity.as_deref().filter(|value| !value.is_empty()) {
        add(severity.to_string(), &|i| format!("e.severity = ?{i}"));
    }
    if let Some(kind) = query.kind.as_deref().filter(|value| !value.is_empty()) {
        /* Prefixes so "cloud_save." selects the whole family, exactly like a
        webhook filter does. */
        add(format!("{kind}%"), &|i| format!("e.kind LIKE ?{i}"));
    }
    if let Some(user_id) = query.user_id.as_deref().filter(|value| !value.is_empty()) {
        add(user_id.to_string(), &|i| format!("e.user_id = ?{i}"));
    }
    if let Some(from) = query.from.as_deref().filter(|value| !value.is_empty()) {
        add(from.to_string(), &|i| format!("e.at >= ?{i}"));
    }
    if let Some(to) = query.to.as_deref().filter(|value| !value.is_empty()) {
        /* A date means the whole day, so compare against its end. */
        add(format!("{to}T23:59:59Z"), &|i| format!("e.at <= ?{i}"));
    }
    if let Some(pattern) = super::like_pattern(query.q.as_deref()) {
        add(pattern, &|i| {
            format!(
                "(e.summary LIKE ?{i} ESCAPE '\\' OR e.kind LIKE ?{i} ESCAPE '\\'
                  OR e.actor LIKE ?{i} ESCAPE '\\' OR e.detail LIKE ?{i} ESCAPE '\\'
                  OR u.display_name LIKE ?{i} ESCAPE '\\' OR g.name LIKE ?{i} ESCAPE '\\')"
            )
        });
    }

    let where_clause = if filters.is_empty() {
        "1 = 1".to_string()
    } else {
        filters.join(" AND ")
    };

    let count_sql = format!("SELECT COUNT(*) {EVENT_JOINS} WHERE {where_clause}");
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql);
    for value in &binds {
        count = count.bind(value);
    }
    let total = count.fetch_one(&state.pool).await?;

    let direction = if query.dir.as_deref() == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    let (limit_slot, offset_slot) = (binds.len() + 1, binds.len() + 2);
    let sql = format!(
        "SELECT {EVENT_COLUMNS} {EVENT_JOINS} WHERE {where_clause}
         ORDER BY e.at {direction}, e.id {direction}
         LIMIT ?{limit_slot} OFFSET ?{offset_slot}"
    );

    let mut rows = sqlx::query(&sql);
    for value in &binds {
        rows = rows.bind(value);
    }
    let rows = rows
        .bind(paging.per_page())
        .bind(paging.offset())
        .fetch_all(&state.pool)
        .await?;

    let events: Vec<Value> = rows.iter().map(crate::events::row_json).collect();

    /* Counts for the current filter, so the severity chips can show how much
    of the result set is a problem. */
    let severity_sql = format!(
        "SELECT e.severity, COUNT(*) {EVENT_JOINS} WHERE {where_clause} GROUP BY e.severity"
    );
    let mut severities = sqlx::query_as::<_, (String, i64)>(&severity_sql);
    for value in &binds {
        severities = severities.bind(value);
    }
    let severities = severities.fetch_all(&state.pool).await?;

    let mut envelope = paging.envelope(events, total);
    envelope["bySeverity"] = json!(severities
        .into_iter()
        .map(|(severity, count)| json!({ "severity": severity, "count": count }))
        .collect::<Vec<_>>());

    Ok(Json(envelope))
}

/// GET /admin/api/events/kinds — the kinds actually present, for the filter
/// dropdown. Derived from the data rather than a hard-coded list, so a kind
/// added later shows up without touching the front end.
async fn kinds(State(state): State<AppState>, _admin: AdminSession) -> ApiResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT kind, category, COUNT(*) AS count FROM events
         GROUP BY kind ORDER BY count DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!(rows
        .iter()
        .map(|row| json!({
            "kind": row.get::<String, _>("kind"),
            "category": row.get::<String, _>("category"),
            "count": row.get::<i64, _>("count"),
        }))
        .collect::<Vec<_>>())))
}
