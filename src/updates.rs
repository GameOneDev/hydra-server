//! Automatic update checks.
//!
//! `hydra-server` shares its version number with the Hydra launcher it is
//! built for: `hydra-server X.Y.Z` targets Hydra app `X.Y.Z`, so an admin can
//! read client compatibility straight off the version. This module
//! periodically asks GitHub for the latest published server release and
//! records whether a newer one is out, which the admin panel surfaces.

use crate::state::AppState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Version this binary was compiled as (from `Cargo.toml`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sent to GitHub, which rejects API requests without a User-Agent.
const USER_AGENT: &str = concat!("hydra-server/", env!("CARGO_PKG_VERSION"));

/// Release notes are only a preview in the panel; keep them bounded.
const RELEASE_NOTES_MAX: usize = 1000;

/// Latest-release view shared with the admin panel.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// Version this server is running.
    pub current_version: String,
    /// Whether the periodic checker is turned on.
    pub enabled: bool,
    /// `owner/repo` the checker watches.
    pub repo: String,
    /// Latest release version seen on GitHub (no leading `v`), once a check
    /// has succeeded against a repo that has releases.
    pub latest_version: Option<String>,
    /// True when `latest_version` is strictly newer than `current_version`.
    pub update_available: bool,
    /// Release page URL for the latest version.
    pub release_url: Option<String>,
    /// Release notes body from GitHub, trimmed to a preview.
    pub release_notes: Option<String>,
    /// When the latest release was published (RFC3339, as GitHub reports it).
    pub published_at: Option<String>,
    /// When a check last reached GitHub successfully.
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Message from the most recent failed check; cleared on success.
    pub last_error: Option<String>,
}

impl UpdateStatus {
    /// Starting status before the first check runs.
    pub fn initial(enabled: bool, repo: String) -> Self {
        Self {
            current_version: CURRENT_VERSION.to_string(),
            enabled,
            repo,
            latest_version: None,
            update_available: false,
            release_url: None,
            release_notes: None,
            published_at: None,
            last_checked_at: None,
            last_error: None,
        }
    }
}

/// Subset of GitHub's release object we care about.
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
}

/// Spawns the periodic checker. A no-op (beyond a log line) when disabled.
pub fn spawn(state: AppState) {
    if !state.config.update_check_enabled {
        tracing::info!("automatic update checks are disabled (HYDRA_UPDATE_CHECK=0)");
        return;
    }

    let interval = Duration::from_secs(state.config.update_check_interval_hours.max(1) * 3600);

    tracing::info!(
        "checking {} for updates every {}h",
        state.config.update_repo,
        state.config.update_check_interval_hours.max(1)
    );

    tokio::spawn(async move {
        loop {
            check(&state).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// Runs one check now and folds the result into the shared status.
pub async fn check(state: &AppState) -> UpdateStatus {
    let outcome = fetch_latest(state).await;

    let mut status = state.updates.write().await;
    match outcome {
        Ok(Some(release)) => {
            let latest = normalize_version(&release.tag_name);
            status.update_available = is_newer(&latest, CURRENT_VERSION);
            status.latest_version = Some(latest);
            status.release_url = Some(release.html_url);
            status.release_notes = release.body.as_deref().map(trim_notes);
            status.published_at = release.published_at;
            status.last_checked_at = Some(Utc::now());
            status.last_error = None;
        }
        Ok(None) => {
            /* Repo has no published releases yet — nothing to update to. */
            status.update_available = false;
            status.latest_version = None;
            status.release_url = None;
            status.release_notes = None;
            status.published_at = None;
            status.last_checked_at = Some(Utc::now());
            status.last_error = None;
        }
        Err(err) => {
            tracing::warn!("update check failed: {err}");
            status.last_error = Some(err);
        }
    }

    status.clone()
}

async fn fetch_latest(state: &AppState) -> Result<Option<GithubRelease>, String> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        state.config.update_repo
    );

    let response = state
        .http
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|err| format!("could not reach GitHub: {err}"))?;

    /* GitHub answers 404 when a repo has no non-prerelease releases yet; that
       isn't an error worth alarming the admin about. */
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("GitHub returned {}", response.status()));
    }

    let release = response
        .json::<GithubRelease>()
        .await
        .map_err(|err| format!("could not parse GitHub response: {err}"))?;

    Ok(Some(release))
}

/// Drops a leading `v`/`V` and surrounding whitespace from a release tag.
fn normalize_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// Collapses a release body to a bounded preview for the panel.
fn trim_notes(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= RELEASE_NOTES_MAX {
        return trimmed.to_string();
    }
    let mut preview: String = trimmed.chars().take(RELEASE_NOTES_MAX).collect();
    preview.push('…');
    preview
}

/// Parses a `MAJOR.MINOR.PATCH` version, ignoring a leading `v` and any
/// `-prerelease`/`+build` suffix. Missing minor/patch components read as 0.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()
        .unwrap_or_default();

    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `candidate` is a strictly newer release than `current`. A tag we
/// can't read as `X.Y.Z` is still surfaced verbatim by the caller but never
/// announced as an update, so unusual tags don't nag operators.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_versions() {
        assert_eq!(parse_version("4.0.6"), Some((4, 0, 6)));
        assert_eq!(parse_version("v4.0.6"), Some((4, 0, 6)));
        assert_eq!(parse_version(" V4.1 "), Some((4, 1, 0)));
        assert_eq!(parse_version("4"), Some((4, 0, 0)));
        assert_eq!(parse_version("4.0.6-rc1"), Some((4, 0, 6)));
        assert_eq!(parse_version("4.0.6+build.9"), Some((4, 0, 6)));
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("4.0.7", "4.0.6"));
        assert!(is_newer("4.1.0", "4.0.6"));
        assert!(is_newer("5.0.0", "4.9.9"));
        assert!(is_newer("v4.0.7", "4.0.6"));
    }

    #[test]
    fn same_or_older_versions_are_not_updates() {
        assert!(!is_newer("4.0.6", "4.0.6"));
        assert!(!is_newer("v4.0.6", "4.0.6"));
        assert!(!is_newer("4.0.5", "4.0.6"));
        assert!(!is_newer("3.9.9", "4.0.0"));
        /* GitHub's "latest" already excludes prereleases; guard anyway. */
        assert!(!is_newer("4.0.6-rc1", "4.0.6"));
    }

    #[test]
    fn unparseable_tags_never_count_as_updates() {
        assert!(!is_newer("nightly", "4.0.6"));
        assert!(!is_newer("4.0.7", "unknown"));
    }

    #[test]
    fn normalizes_tag_prefix() {
        assert_eq!(normalize_version("v4.0.6"), "4.0.6");
        assert_eq!(normalize_version(" 4.0.6 "), "4.0.6");
    }

    #[test]
    fn trims_long_release_notes() {
        let long = "x".repeat(RELEASE_NOTES_MAX + 50);
        let trimmed = trim_notes(&long);
        assert_eq!(trimmed.chars().count(), RELEASE_NOTES_MAX + 1);
        assert!(trimmed.ends_with('…'));

        assert_eq!(trim_notes("  short  "), "short");
    }

    #[test]
    fn decides_update_from_a_github_release_payload() {
        /* Shape mirrors GitHub's /releases/latest response; extra fields the
           struct doesn't name are ignored. */
        let payload = r#"{
            "tag_name": "v4.1.0",
            "html_url": "https://github.com/gameonedev/hydra-server/releases/tag/v4.1.0",
            "body": "What's new: stuff",
            "published_at": "2026-07-01T00:00:00Z",
            "prerelease": false
        }"#;

        let release: GithubRelease = serde_json::from_str(payload).unwrap();
        let latest = normalize_version(&release.tag_name);
        assert_eq!(latest, "4.1.0");
        assert!(is_newer(&latest, "4.0.6"));
        assert!(!is_newer(&latest, "4.1.0"));
        assert_eq!(release.published_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(release.body.as_deref(), Some("What's new: stuff"));
    }

    #[test]
    fn release_payload_tolerates_missing_optional_fields() {
        let payload = r#"{"tag_name": "4.0.6", "html_url": "https://example.test/x"}"#;
        let release: GithubRelease = serde_json::from_str(payload).unwrap();
        assert_eq!(release.body, None);
        assert_eq!(release.published_at, None);
        assert!(!is_newer(&normalize_version(&release.tag_name), CURRENT_VERSION));
    }
}
