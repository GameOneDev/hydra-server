//! Souvenir reports: what players flagged, and what the operator did about it.
//!
//! Players report a picture, not a person, so the screen works per souvenir:
//! every report on the same picture is one row, and one decision settles all
//! of them. Reports outlive the picture on purpose (see the migration), so a
//! row can describe a souvenir that is already gone.

use super::{AdminSession, Paging};
use crate::error::{ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/api/reports", get(list))
        .route("/admin/api/reports/{souvenir_id}", get(detail))
        .route("/admin/api/reports/{souvenir_id}/resolve", post(resolve))
}

/// One row per reported picture: what it is, whose it is, and how its
/// reports stand. A picture with any open report is open.
const REPORTED_SOUVENIRS: &str = "
    SELECT r.souvenir_id,
           MAX(r.owner_user_id) AS user_id,
           COUNT(*) AS report_count,
           SUM(r.status = 'open') AS open_count,
           MIN(r.created_at) AS first_reported_at,
           MAX(r.created_at) AS last_reported_at,
           MAX(r.resolved_at) AS resolved_at,
           GROUP_CONCAT(DISTINCT r.reason) AS reasons,
           (SELECT x.status FROM souvenir_reports x
             WHERE x.souvenir_id = r.souvenir_id AND x.status <> 'open'
             ORDER BY x.resolved_at DESC LIMIT 1) AS resolution,
           v.id AS present_id, v.image_key, v.visibility, v.shop, v.object_id,
           v.primary_achievement_name, v.captured_at,
           u.display_name, u.username, u.profile_image_url,
           g.name AS game_name, g.cover_url AS game_cover_url
    FROM souvenir_reports r
    LEFT JOIN souvenirs v ON v.id = r.souvenir_id
    LEFT JOIN users u ON u.id = r.owner_user_id
    LEFT JOIN game_metadata g ON g.shop = v.shop AND g.object_id = v.object_id
";

fn status_filter(status: &str) -> &'static str {
    match status {
        "resolved" => "HAVING open_count = 0",
        "all" => "",
        _ => "HAVING open_count > 0",
    }
}

fn image_url(state: &AppState, key: Option<String>) -> Option<String> {
    key.map(|key| {
        format!(
            "{}/{}",
            state.config.public_url,
            key.trim_start_matches('/')
        )
    })
}

fn reported_souvenir_json(state: &AppState, row: &sqlx::sqlite::SqliteRow) -> Value {
    let open: i64 = row.get("open_count");

    json!({
        "souvenirId": row.get::<String, _>("souvenir_id"),
        "owner": super::user_ref(row),
        "game": super::game_ref(row),
        /* False once the picture is gone: deleted from the profile by its
           owner, or by an earlier decision here. */
        "present": row.get::<Option<String>, _>("present_id").is_some(),
        "url": image_url(state, row.get("image_key")),
        "visibility": row.get::<Option<String>, _>("visibility"),
        "achievementName": row.get::<Option<String>, _>("primary_achievement_name"),
        "reportCount": row.get::<i64, _>("report_count"),
        "openCount": open,
        "reasons": row
            .get::<Option<String>, _>("reasons")
            .map(|reasons| reasons.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default(),
        "firstReportedAt": row.get::<String, _>("first_reported_at"),
        "lastReportedAt": row.get::<String, _>("last_reported_at"),
        "status": if open > 0 { "open".to_string() } else {
            row.get::<Option<String>, _>("resolution").unwrap_or_else(|| "open".to_string())
        },
        "resolvedAt": if open > 0 { None } else { row.get::<Option<String>, _>("resolved_at") },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    /// `open` (the default), `resolved` or `all`.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    per_page: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    _admin: AdminSession,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let paging = Paging::new(query.page, query.per_page);
    let filter = status_filter(query.status.as_deref().unwrap_or("open"));
    let grouped = format!("{REPORTED_SOUVENIRS} GROUP BY r.souvenir_id {filter}");

    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM ({grouped})"))
        .fetch_one(&state.pool)
        .await?;

    let rows = sqlx::query(&format!(
        "{grouped} ORDER BY open_count > 0 DESC, last_reported_at DESC LIMIT ? OFFSET ?"
    ))
    .bind(paging.per_page())
    .bind(paging.offset())
    .fetch_all(&state.pool)
    .await?;

    let mut body = paging.envelope(
        rows.iter()
            .map(|row| reported_souvenir_json(&state, row))
            .collect(),
        total,
    );
    body["openTotal"] = json!(open_total(&state).await?);

    Ok(Json(body))
}

/// Pictures with at least one report nobody has acted on — what the
/// Overview alert and the navigation count show.
pub(crate) async fn open_total(state: &AppState) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(DISTINCT souvenir_id) FROM souvenir_reports WHERE status = 'open'",
    )
    .fetch_one(&state.pool)
    .await?)
}

/// The picture and every report filed against it, reporter included.
async fn detail(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(souvenir_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let summary = sqlx::query(&format!(
        "{REPORTED_SOUVENIRS} WHERE r.souvenir_id = ? GROUP BY r.souvenir_id"
    ))
    .bind(&souvenir_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("no reports for that souvenir"))?;

    let reports = sqlx::query(
        "SELECT r.id, r.reason, r.description, r.status, r.created_at, r.resolved_at,
                r.reporter_user_id AS user_id,
                u.display_name, u.username, u.profile_image_url
         FROM souvenir_reports r
         LEFT JOIN users u ON u.id = r.reporter_user_id
         WHERE r.souvenir_id = ?
         ORDER BY r.created_at DESC",
    )
    .bind(&souvenir_id)
    .fetch_all(&state.pool)
    .await?;

    let mut body = reported_souvenir_json(&state, &summary);
    body["reports"] = json!(reports
        .iter()
        .map(|row| json!({
            "id": row.get::<i64, _>("id"),
            "reporter": super::user_ref(row),
            "reason": row.get::<String, _>("reason"),
            "description": row.get::<Option<String>, _>("description"),
            "status": row.get::<String, _>("status"),
            "createdAt": row.get::<String, _>("created_at"),
            "resolvedAt": row.get::<Option<String>, _>("resolved_at"),
        }))
        .collect::<Vec<_>>());

    Ok(Json(body))
}

#[derive(Deserialize)]
struct ResolveRequest {
    /// `dismiss`, `hide`, `delete` or `reopen`.
    action: String,
}

/// Settles every report on one picture with a single decision.
///
/// - `dismiss` — nothing wrong; the picture stays as it is.
/// - `hide` — the picture is made private, so it leaves the owner's profile
///   for everyone else but stays theirs. The owner can make it public again
///   from the launcher, which is why `delete` exists.
/// - `delete` — the picture and its file are removed, freeing the owner's
///   quota.
/// - `reopen` — back to open, for a decision made too quickly.
async fn resolve(
    State(state): State<AppState>,
    _admin: AdminSession,
    Path(souvenir_id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> ApiResult<Json<Value>> {
    let owner: String = sqlx::query_scalar(
        "SELECT owner_user_id FROM souvenir_reports WHERE souvenir_id = ? LIMIT 1",
    )
    .bind(&souvenir_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("no reports for that souvenir"))?;

    let now = Utc::now().to_rfc3339();

    let (status, kind, summary, freed) = match request.action.as_str() {
        "dismiss" => (
            "dismissed",
            "admin.report.dismissed",
            "Dismissed the reports on a souvenir",
            None,
        ),
        "hide" => {
            let hidden = sqlx::query(
                "UPDATE souvenirs SET visibility = 'PRIVATE', updated_at = ?
                 WHERE id = ? AND user_id = ?",
            )
            .bind(&now)
            .bind(&souvenir_id)
            .bind(&owner)
            .execute(&state.pool)
            .await?
            .rows_affected();

            if hidden == 0 {
                return Err(ApiError::not_found(
                    "that souvenir no longer exists — dismiss its reports instead",
                ));
            }

            (
                "hidden",
                "admin.souvenir.hidden",
                "Hid a reported souvenir from its owner's profile",
                None,
            )
        }
        "delete" => {
            /* Already gone counts as done: the owner may have deleted it
            since it was reported, and the reports still need settling. */
            let freed = match crate::souvenirs::remove(&state, &owner, &souvenir_id).await {
                Ok(freed) => freed,
                Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => 0,
                Err(error) => return Err(error),
            };

            (
                "removed",
                "admin.souvenir.removed",
                "Deleted a reported souvenir",
                Some(freed),
            )
        }
        "reopen" => (
            "open",
            "admin.report.reopened",
            "Reopened the reports on a souvenir",
            None,
        ),
        _ => {
            return Err(ApiError::bad_request(
                "action must be dismiss, hide, delete or reopen",
            ))
        }
    };

    let resolved_at = (status != "open").then_some(now.as_str());
    let reports = sqlx::query(
        "UPDATE souvenir_reports SET status = ?, resolved_at = ? WHERE souvenir_id = ?",
    )
    .bind(status)
    .bind(resolved_at)
    .bind(&souvenir_id)
    .execute(&state.pool)
    .await?
    .rows_affected();

    let mut event = Event::admin(kind, summary)
        .about(&owner)
        .detail(json!({ "souvenirId": souvenir_id, "reports": reports }));
    if let Some(freed) = freed {
        event = event.size(freed);
    }
    crate::events::record(&state, event).await;

    Ok(Json(
        json!({ "ok": true, "status": status, "reports": reports }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestServer;

    const SOUVENIR: &str = "souvenir-1";

    /// A ready souvenir of alice's with its picture on disk, reported by
    /// bob and carol.
    async fn a_reported_souvenir(server: &TestServer) {
        server.add_user("bob").await;
        server.add_user("carol").await;

        let key = "images/souvenirs/alice/picture.jpg";
        server
            .execute(&format!(
                "INSERT INTO souvenirs
                   (id, user_id, client_id, shop, object_id, image_key, is_uploaded,
                    size_in_bytes, status, captured_at, visibility, created_at, updated_at)
                 VALUES ('{SOUVENIR}', 'alice', 'c1', 'steam', '440', '{key}', 1,
                         5, 'ready', 0, 'PUBLIC', '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')"
            ))
            .await;

        let path = crate::storage::storage_path(&server.state, key);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the image dir");
        std::fs::write(&path, b"image").expect("the picture");

        for (reporter, reason, at) in [
            ("bob", "spam", "2026-01-02T00:00:00Z"),
            ("carol", "violence", "2026-01-03T00:00:00Z"),
        ] {
            server
                .execute(&format!(
                    "INSERT INTO souvenir_reports
                       (souvenir_id, owner_user_id, reporter_user_id, reason, created_at)
                     VALUES ('{SOUVENIR}', 'alice', '{reporter}', '{reason}', '{at}')"
                ))
                .await;
        }
    }

    async fn listing(server: &TestServer, status: &str) -> Value {
        let Json(value) = list(
            State(server.state.clone()),
            AdminSession { expires_at: 0 },
            Query(ListQuery {
                status: Some(status.to_string()),
                page: None,
                per_page: None,
            }),
        )
        .await
        .expect("the listing");
        value
    }

    async fn act(server: &TestServer, action: &str) -> ApiResult<Json<Value>> {
        resolve(
            State(server.state.clone()),
            AdminSession { expires_at: 0 },
            Path(SOUVENIR.to_string()),
            Json(ResolveRequest {
                action: action.to_string(),
            }),
        )
        .await
    }

    #[tokio::test]
    async fn reports_on_one_picture_are_one_open_row() {
        let server = TestServer::start().await;
        a_reported_souvenir(&server).await;

        let open = listing(&server, "open").await;
        assert_eq!(open["total"], 1);
        assert_eq!(open["openTotal"], 1);

        let row = &open["rows"][0];
        assert_eq!(row["reportCount"], 2);
        assert_eq!(row["status"], "open");
        assert_eq!(row["present"], true);
        assert_eq!(row["owner"]["id"], "alice");

        let Json(detail) = detail(
            State(server.state.clone()),
            AdminSession { expires_at: 0 },
            Path(SOUVENIR.to_string()),
        )
        .await
        .expect("the detail");
        assert_eq!(detail["reports"][0]["reporter"]["id"], "carol");
        assert_eq!(detail["reports"][1]["reason"], "spam");
    }

    #[tokio::test]
    async fn dismissing_moves_the_picture_to_resolved_and_reopen_brings_it_back() {
        let server = TestServer::start().await;
        a_reported_souvenir(&server).await;

        let _ = act(&server, "dismiss").await.expect("the dismissal");

        assert_eq!(listing(&server, "open").await["total"], 0);
        let resolved = listing(&server, "resolved").await;
        assert_eq!(resolved["rows"][0]["status"], "dismissed");
        assert_eq!(resolved["openTotal"], 0);
        assert_eq!(
            server
                .scalar::<String>("SELECT visibility FROM souvenirs")
                .await,
            "PUBLIC"
        );

        let _ = act(&server, "reopen").await.expect("the reopening");
        assert_eq!(listing(&server, "open").await["total"], 1);
    }

    #[tokio::test]
    async fn hiding_makes_the_picture_private() {
        let server = TestServer::start().await;
        a_reported_souvenir(&server).await;

        let _ = act(&server, "hide").await.expect("the hiding");

        assert_eq!(
            server
                .scalar::<String>("SELECT visibility FROM souvenirs")
                .await,
            "PRIVATE"
        );
        assert_eq!(
            listing(&server, "resolved").await["rows"][0]["status"],
            "hidden"
        );
    }

    #[tokio::test]
    async fn deleting_removes_the_picture_and_keeps_the_record() {
        let server = TestServer::start().await;
        a_reported_souvenir(&server).await;

        let _ = act(&server, "delete").await.expect("the deletion");

        assert_eq!(
            server.scalar::<i64>("SELECT COUNT(*) FROM souvenirs").await,
            0
        );
        assert!(
            !crate::storage::storage_path(&server.state, "images/souvenirs/alice/picture.jpg")
                .exists()
        );

        let resolved = listing(&server, "resolved").await;
        assert_eq!(resolved["rows"][0]["status"], "removed");
        assert_eq!(resolved["rows"][0]["present"], false);

        /* Deleting again — or a picture its owner already deleted — still
        settles the reports rather than failing. */
        let _ = act(&server, "delete").await.expect("a second deletion");
        /* There is nothing left to hide. */
        assert!(act(&server, "hide").await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused() {
        let server = TestServer::start().await;
        a_reported_souvenir(&server).await;

        assert!(act(&server, "ban").await.is_err());
        assert_eq!(listing(&server, "open").await["total"], 1);
    }
}
