//! Outbound webhooks.
//!
//! Every recorded event is offered to each enabled hook whose filters match.
//! Delivery happens on a detached task: a slow or dead endpoint must not hold
//! up the upload that triggered it, and a hook that keeps failing is disabled
//! rather than retried forever.

use crate::events::{Event, Severity};
use crate::state::AppState;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::Row;

/// A hook that fails this many times in a row is switched off. Something is
/// wrong with the endpoint, and a self-hosted server should not spend the
/// rest of its life proving it.
const FAILURE_LIMIT: i64 = 20;

const TIMEOUT_SECONDS: u64 = 10;

/// One configured endpoint.
pub struct Webhook {
    pub id: String,
    pub label: String,
    pub url: String,
    pub format: String,
    pub secret: Option<String>,
    pub kinds: Vec<String>,
    pub min_severity: Severity,
}

fn from_row(row: &sqlx::sqlite::SqliteRow) -> Webhook {
    Webhook {
        id: row.get("id"),
        label: row.get("label"),
        url: row.get("url"),
        format: row.get("format"),
        secret: row.get("secret"),
        kinds: serde_json::from_str(&row.get::<String, _>("kinds")).unwrap_or_default(),
        min_severity: Severity::parse(&row.get::<String, _>("min_severity")),
    }
}

impl Webhook {
    /// Kinds are matched as prefixes, so `cloud_save.` catches every cloud
    /// save event and `admin.user.deleted` catches exactly one.
    fn matches(&self, event: &Event) -> bool {
        if event.severity < self.min_severity {
            return false;
        }
        self.kinds.is_empty()
            || self
                .kinds
                .iter()
                .any(|filter| event.kind.starts_with(filter.as_str()))
    }
}

/// The JSON body a `json` hook receives. Stable shape — someone will script
/// against it.
pub fn payload(state: &AppState, event: &Event, at: &str) -> Value {
    json!({
        "server": {
            "name": "hydra-server",
            "version": env!("CARGO_PKG_VERSION"),
            "publicUrl": state.config.public_url,
        },
        "event": {
            "at": at,
            "kind": event.kind,
            "severity": event.severity.as_str(),
            "actor": event.actor,
            "userId": event.user_id,
            "shop": event.shop,
            "objectId": event.object_id,
            "summary": event.summary,
            "detail": event.detail,
            "sizeBytes": event.size_bytes,
        },
    })
}

/// Chat services want a message, not a document.
fn body_for(format: &str, state: &AppState, event: &Event, at: &str) -> Value {
    let icon = match event.severity {
        Severity::Critical => "🔴",
        Severity::Warning => "🟠",
        Severity::Info => "🔵",
    };
    let text = format!("{icon} **{}** — {}", event.kind, event.summary);

    match format {
        "discord" => json!({ "content": text }),
        "slack" => json!({ "text": text }),
        _ => payload(state, event, at),
    }
}

fn sign(secret: &str, body: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Offers an event to every matching hook, on a detached task.
pub async fn dispatch(state: &AppState, event: &Event, at: &str) {
    let rows = sqlx::query("SELECT * FROM webhooks WHERE enabled = 1")
        .fetch_all(&state.pool)
        .await;

    let Ok(rows) = rows else {
        return;
    };

    let hooks: Vec<Webhook> = rows
        .iter()
        .map(from_row)
        .filter(|hook| hook.matches(event))
        .collect();

    if hooks.is_empty() {
        return;
    }

    /* Bodies are rendered here, while the event is still borrowed; the task
    below only needs the finished strings. */
    let deliveries: Vec<(Webhook, String)> = hooks
        .into_iter()
        .map(|hook| {
            let body = body_for(&hook.format, state, event, at).to_string();
            (hook, body)
        })
        .collect();

    let state = state.clone();
    tokio::spawn(async move {
        for (hook, body) in deliveries {
            deliver(&state, &hook, body).await;
        }
    });
}

async fn deliver(state: &AppState, hook: &Webhook, body: String) {
    let mut request = state
        .http
        .post(&hook.url)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .header("content-type", "application/json")
        .header(
            "user-agent",
            concat!("hydra-server/", env!("CARGO_PKG_VERSION")),
        );

    if let Some(secret) = &hook.secret {
        request = request.header("x-hydra-signature", sign(secret, &body));
    }

    let outcome = match request.body(body).send().await {
        Ok(response) if response.status().is_success() => Ok(response.status().as_u16()),
        Ok(response) => Err(format!("HTTP {}", response.status())),
        Err(err) => Err(err.to_string()),
    };

    let now = chrono::Utc::now().to_rfc3339();
    let query = match outcome {
        Ok(status) => sqlx::query(
            "UPDATE webhooks
             SET last_delivery_at = ?, last_status = ?, last_error = NULL,
                 delivered_count = delivered_count + 1, failure_count = 0
             WHERE id = ?",
        )
        .bind(&now)
        .bind(status.to_string())
        .bind(&hook.id),
        Err(error) => {
            tracing::warn!(
                "webhook {} failed: {error}",
                if hook.label.is_empty() {
                    &hook.url
                } else {
                    &hook.label
                }
            );
            sqlx::query(
                "UPDATE webhooks
                 SET last_delivery_at = ?, last_status = 'failed', last_error = ?,
                     failure_count = failure_count + 1,
                     enabled = CASE WHEN failure_count + 1 >= ? THEN 0 ELSE enabled END
                 WHERE id = ?",
            )
            .bind(&now)
            .bind(error)
            .bind(FAILURE_LIMIT)
            .bind(&hook.id)
        }
    };

    if let Err(err) = query.execute(&state.pool).await {
        tracing::warn!("failed to record webhook delivery: {err}");
    }
}

/// Sends a synthetic event to one hook and reports what happened, so the
/// panel's "Test" button answers the only question that matters: does this
/// URL actually accept what we send it.
pub async fn test(state: &AppState, id: &str) -> Result<Value, String> {
    let row = sqlx::query("SELECT * FROM webhooks WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "webhook not found".to_string())?;

    let hook = from_row(&row);
    let at = chrono::Utc::now().to_rfc3339();
    let event = Event::system("system.webhook_test", "Test delivery from the admin panel");
    let body = body_for(&hook.format, state, &event, &at).to_string();

    let mut request = state
        .http
        .post(&hook.url)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .header("content-type", "application/json");
    if let Some(secret) = &hook.secret {
        request = request.header("x-hydra-signature", sign(secret, &body));
    }

    let started = std::time::Instant::now();
    let result = request.body(body).send().await;
    let elapsed = started.elapsed().as_millis() as i64;
    let now = chrono::Utc::now().to_rfc3339();

    match result {
        Ok(response) => {
            let status = response.status();
            let ok = status.is_success();
            let _ = sqlx::query(
                "UPDATE webhooks SET last_delivery_at = ?, last_status = ?,
                    last_error = ?, failure_count = CASE WHEN ? THEN 0 ELSE failure_count + 1 END
                 WHERE id = ?",
            )
            .bind(&now)
            .bind(status.as_u16().to_string())
            .bind(if ok {
                None
            } else {
                Some(format!("HTTP {status}"))
            })
            .bind(ok)
            .bind(id)
            .execute(&state.pool)
            .await;

            Ok(json!({
                "ok": ok,
                "status": status.as_u16(),
                "elapsedMs": elapsed,
            }))
        }
        Err(err) => {
            let error = err.to_string();
            let _ = sqlx::query(
                "UPDATE webhooks SET last_delivery_at = ?, last_status = 'failed',
                    last_error = ?, failure_count = failure_count + 1 WHERE id = ?",
            )
            .bind(&now)
            .bind(&error)
            .bind(id)
            .execute(&state.pool)
            .await;

            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(kinds: &[&str], min: Severity) -> Webhook {
        Webhook {
            id: "id".into(),
            label: String::new(),
            url: "http://example.test".into(),
            format: "json".into(),
            secret: None,
            kinds: kinds.iter().map(|kind| (*kind).to_string()).collect(),
            min_severity: min,
        }
    }

    #[test]
    fn no_filters_means_everything() {
        let event = Event::system("system.started", "up");
        assert!(hook(&[], Severity::Info).matches(&event));
    }

    #[test]
    fn kinds_match_as_prefixes() {
        let commit = Event::sync("cloud_save.committed", "u", "synced");
        let deleted = Event::admin("admin.user.deleted", "gone");

        assert!(hook(&["cloud_save."], Severity::Info).matches(&commit));
        assert!(!hook(&["cloud_save."], Severity::Info).matches(&deleted));
        assert!(hook(&["admin.user.deleted"], Severity::Info).matches(&deleted));
        assert!(hook(&["backup.", "admin."], Severity::Info).matches(&deleted));
    }

    #[test]
    fn severity_is_a_floor_not_a_match() {
        let info = Event::system("system.gc", "collected");
        let critical = Event::system("system.integrity", "missing bytes").critical();

        assert!(!hook(&[], Severity::Warning).matches(&info));
        assert!(hook(&[], Severity::Warning).matches(&critical));
        assert!(hook(&[], Severity::Info).matches(&critical));
    }

    /// The signature is what lets a receiver trust the body; pin the exact
    /// construction so a refactor can't quietly change it.
    #[test]
    fn signature_is_hex_hmac_sha256_of_the_body() {
        assert_eq!(
            sign("secret", "{}"),
            "sha256=77325902caca812dc259733aacd046b73817372c777b8d95b402647474516e13"
        );
    }
}
