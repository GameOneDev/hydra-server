//! Update checks and (optional) self-install.
//!
//! `hydra-server` shares its version number with the Hydra launcher it is
//! built for: `hydra-server X.Y.Z` targets Hydra app `X.Y.Z`, so an admin can
//! read client compatibility straight off the version. This module
//! periodically asks GitHub for the latest published server release and
//! records whether a newer one is out, which the admin panel surfaces.
//!
//! Detecting an update and *installing* it are separate concerns:
//!
//! * The **checker** (env `HYDRA_UPDATE_CHECK`, on by default) only notifies.
//! * **Auto-install** (the `auto_update` runtime setting, off by default) and
//!   the panel's "Update now" button call [`install`], which downloads the
//!   matching release binary, swaps it in place and re-execs. Inside a
//!   container that swap is pointless — the image would revert on restart — so
//!   there the installer refuses and points the admin at pulling a new image.

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

/// Install progress, surfaced to the panel. Plain strings keep the JSON and
/// the front-end simple.
pub const INSTALL_IDLE: &str = "idle";
pub const INSTALL_RUNNING: &str = "installing";
pub const INSTALL_RESTARTING: &str = "restarting";
pub const INSTALL_FAILED: &str = "failed";

/// Latest-release view shared with the admin panel.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// Version this server is running.
    pub current_version: String,
    /// Whether the periodic checker is turned on.
    pub enabled: bool,
    /// Whether detected updates install themselves (the `auto_update` setting).
    pub auto_update: bool,
    /// True when running inside a container, where self-install can't persist.
    pub containerized: bool,
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
    /// One of the `INSTALL_*` constants.
    pub install_state: String,
    /// Message from the most recent failed/blocked install; cleared on start.
    pub install_error: Option<String>,
    /// Version auto-install last tried, so a failing auto-install doesn't
    /// retry every interval. Manual installs ignore it. Not sent to the panel.
    #[serde(skip)]
    auto_attempted_version: Option<String>,
}

impl UpdateStatus {
    /// Starting status before the first check runs.
    pub fn initial(enabled: bool, auto_update: bool, containerized: bool, repo: String) -> Self {
        Self {
            current_version: CURRENT_VERSION.to_string(),
            enabled,
            auto_update,
            containerized,
            repo,
            latest_version: None,
            update_available: false,
            release_url: None,
            release_notes: None,
            published_at: None,
            last_checked_at: None,
            last_error: None,
            install_state: INSTALL_IDLE.to_string(),
            install_error: None,
            auto_attempted_version: None,
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
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

/// A downloadable file attached to a release.
#[derive(Deserialize, Clone)]
struct GithubAsset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
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

/// Runs one check now and folds the result into the shared status. When
/// auto-install is on and a fresh, installable update appears, kicks it off.
pub async fn check(state: &AppState) -> UpdateStatus {
    let auto = state.settings.read().await.auto_update;
    let outcome = fetch_latest(state).await;

    let (auto_install, snapshot) = {
        let mut status = state.updates.write().await;
        status.auto_update = auto;

        match outcome {
            Ok(Some(release)) => {
                let latest = normalize_version(&release.tag_name);
                status.update_available = is_newer(&latest, CURRENT_VERSION);
                status.latest_version = Some(latest.clone());
                status.release_url = Some(release.html_url);
                status.release_notes = release.body.as_deref().map(trim_notes);
                status.published_at = release.published_at;
                status.last_checked_at = Some(Utc::now());
                status.last_error = None;

                /* Auto-install only a version we haven't already tried, and
                   never while a swap is mid-flight or inside a container. */
                let installing = status.install_state == INSTALL_RUNNING
                    || status.install_state == INSTALL_RESTARTING;
                let auto_install = status.update_available
                    && auto
                    && !status.containerized
                    && !installing
                    && status.auto_attempted_version.as_deref() != Some(latest.as_str());
                if auto_install {
                    status.auto_attempted_version = Some(latest);
                }
                (auto_install, status.clone())
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
                (false, status.clone())
            }
            Err(err) => {
                tracing::warn!("update check failed: {err}");
                status.last_error = Some(err);
                (false, status.clone())
            }
        }
    };

    if auto_install {
        tracing::info!("auto-update on and a newer release is out — installing");
        return install(state).await;
    }

    snapshot
}

/// Downloads the latest release's matching binary, swaps it in and schedules a
/// restart. Guarded so only one install runs at a time; refuses inside a
/// container. Returns the updated status either way.
pub async fn install(state: &AppState) -> UpdateStatus {
    {
        let mut status = state.updates.write().await;
        if status.install_state == INSTALL_RUNNING || status.install_state == INSTALL_RESTARTING {
            return status.clone();
        }
        if !status.update_available {
            status.install_error = Some("already up to date".to_string());
            return status.clone();
        }
        if status.containerized {
            status.install_state = INSTALL_FAILED.to_string();
            status.install_error = Some(
                "running in a container — update by pulling the new image \
                 (docker compose pull && docker compose up -d)"
                    .to_string(),
            );
            return status.clone();
        }
        status.install_state = INSTALL_RUNNING.to_string();
        status.install_error = None;
    }

    let outcome = perform_install(state).await;

    let mut status = state.updates.write().await;
    match outcome {
        Ok(()) => {
            status.install_state = INSTALL_RESTARTING.to_string();
            status.install_error = None;
            schedule_restart();
        }
        Err(err) => {
            tracing::error!("update install failed: {err}");
            status.install_state = INSTALL_FAILED.to_string();
            status.install_error = Some(err);
        }
    }
    status.clone()
}

/// Fetches the release, picks the binary for this platform, downloads it and
/// replaces the running executable. Leaves the restart to the caller.
async fn perform_install(state: &AppState) -> Result<(), String> {
    let release = fetch_latest(state)
        .await?
        .ok_or_else(|| "the repository has no published releases".to_string())?;

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let (url, expected_size) = {
        let asset = select_asset(&release.assets, os, arch).ok_or_else(|| {
            format!(
                "release {} has no downloadable binary for {os}/{arch}",
                release.tag_name
            )
        })?;
        (asset.browser_download_url.clone(), asset.size)
    };

    tracing::info!("downloading update for {os}/{arch} from {url}");
    let bytes = download_asset(state, &url).await?;

    if bytes.is_empty() {
        return Err("downloaded an empty file".to_string());
    }
    if expected_size > 0 && bytes.len() as u64 != expected_size {
        return Err(format!(
            "downloaded {} bytes but the release lists {expected_size}",
            bytes.len()
        ));
    }

    replace_running_binary(&bytes)
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

async fn download_asset(state: &AppState, url: &str) -> Result<Vec<u8>, String> {
    let response = state
        .http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|err| format!("could not download the update: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("download returned {}", response.status()));
    }

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| format!("could not read the download: {err}"))
}

/// Picks the release asset that is a runnable binary for `os`/`arch`. Asset
/// names are expected to carry both, e.g. `hydra-server-linux-x86_64`.
fn select_asset<'a>(assets: &'a [GithubAsset], os: &str, arch: &str) -> Option<&'a GithubAsset> {
    let os_tokens = os_tokens(os);
    let arch_tokens = arch_tokens(arch);
    if os_tokens.is_empty() || arch_tokens.is_empty() {
        return None;
    }

    assets.iter().find(|asset| {
        if asset.browser_download_url.is_empty() {
            return false;
        }
        let name = asset.name.to_lowercase();
        /* Checksums and signatures sit next to the binary — skip them. */
        if [".sha256", ".asc", ".sig", ".txt", ".pem"]
            .iter()
            .any(|ext| name.ends_with(ext))
        {
            return false;
        }
        os_tokens.iter().any(|token| name.contains(token))
            && arch_tokens.iter().any(|token| name.contains(token))
    })
}

fn os_tokens(os: &str) -> &'static [&'static str] {
    match os {
        "linux" => &["linux"],
        "macos" => &["darwin", "macos", "apple"],
        "windows" => &["windows", "win"],
        _ => &[],
    }
}

fn arch_tokens(arch: &str) -> &'static [&'static str] {
    match arch {
        "x86_64" => &["x86_64", "amd64", "x64"],
        "aarch64" => &["aarch64", "arm64"],
        "arm" => &["armv7", "armhf", "arm"],
        _ => &[],
    }
}

/// Replaces the running executable with `bytes`. The current binary is moved
/// aside first (allowed on Unix while running) so a failed swap can roll back.
#[cfg(unix)]
fn replace_running_binary(bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot locate the current executable: {err}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "the executable has no parent directory".to_string())?;

    let staged = dir.join(".hydra-server.update");
    std::fs::write(&staged, bytes).map_err(|err| format!("writing the new binary: {err}"))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("making the new binary executable: {err}"))?;

    let backup = dir.join(".hydra-server.old");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&exe, &backup).map_err(|err| {
        let _ = std::fs::remove_file(&staged);
        format!("moving the current binary aside: {err}")
    })?;

    if let Err(err) = std::fs::rename(&staged, &exe) {
        /* Put the working binary back so we don't leave the server headless. */
        let _ = std::fs::rename(&backup, &exe);
        let _ = std::fs::remove_file(&staged);
        return Err(format!("installing the new binary: {err}"));
    }

    /* The old inode stays alive for the running process; unlink the name. */
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

#[cfg(not(unix))]
fn replace_running_binary(_bytes: &[u8]) -> Result<(), String> {
    Err("self-install is only supported on Unix; please update manually".to_string())
}

/// Re-execs the freshly installed binary after a beat, so the HTTP response
/// that triggered the install can flush first.
#[cfg(unix)]
fn schedule_restart() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;

        use std::os::unix::process::CommandExt;
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                tracing::error!("restart: cannot locate executable: {err}");
                std::process::exit(0);
            }
        };

        tracing::info!("restarting into the updated binary");
        let err = std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec();

        /* exec only returns on failure; exit so a supervisor can restart us. */
        tracing::error!("re-exec failed: {err}; exiting for the supervisor to restart");
        std::process::exit(0);
    });
}

#[cfg(not(unix))]
fn schedule_restart() {
    tracing::warn!("restart is not supported on this platform; please restart manually");
}

/// Whether the process looks like it's running inside a container, where an
/// in-place binary swap wouldn't survive a restart.
pub fn detect_container() -> bool {
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return true;
    }
    std::fs::read_to_string("/proc/1/cgroup")
        .map(|cgroup| cgroup_indicates_container(&cgroup))
        .unwrap_or(false)
}

fn cgroup_indicates_container(cgroup: &str) -> bool {
    let cgroup = cgroup.to_lowercase();
    ["docker", "containerd", "kubepods", "/lxc/", "podman"]
        .iter()
        .any(|marker| cgroup.contains(marker))
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

    fn asset(name: &str) -> GithubAsset {
        GithubAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.test/{name}"),
            size: 10,
        }
    }

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
            "prerelease": false,
            "assets": [
                { "name": "hydra-server-linux-x86_64", "browser_download_url": "https://example.test/bin", "size": 1234 }
            ]
        }"#;

        let release: GithubRelease = serde_json::from_str(payload).unwrap();
        let latest = normalize_version(&release.tag_name);
        assert_eq!(latest, "4.1.0");
        assert!(is_newer(&latest, "4.0.6"));
        assert!(!is_newer(&latest, "4.1.0"));
        assert_eq!(release.published_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(release.body.as_deref(), Some("What's new: stuff"));
        assert_eq!(release.assets.len(), 1);
    }

    #[test]
    fn release_payload_tolerates_missing_optional_fields() {
        let payload = r#"{"tag_name": "4.0.6", "html_url": "https://example.test/x"}"#;
        let release: GithubRelease = serde_json::from_str(payload).unwrap();
        assert_eq!(release.body, None);
        assert_eq!(release.published_at, None);
        assert!(release.assets.is_empty());
        assert!(!is_newer(&normalize_version(&release.tag_name), CURRENT_VERSION));
    }

    #[test]
    fn selects_the_binary_for_the_platform() {
        let assets = vec![
            asset("hydra-server-linux-x86_64"),
            asset("hydra-server-linux-aarch64"),
            asset("hydra-server-darwin-arm64"),
            asset("hydra-server-linux-x86_64.sha256"),
        ];

        let linux_x64 = select_asset(&assets, "linux", "x86_64").unwrap();
        assert_eq!(linux_x64.name, "hydra-server-linux-x86_64");

        let linux_arm = select_asset(&assets, "linux", "aarch64").unwrap();
        assert_eq!(linux_arm.name, "hydra-server-linux-aarch64");

        let mac_arm = select_asset(&assets, "macos", "aarch64").unwrap();
        assert_eq!(mac_arm.name, "hydra-server-darwin-arm64");
    }

    #[test]
    fn tolerates_common_arch_and_os_aliases() {
        let assets = vec![asset("hydra-server_linux_amd64.bin")];
        assert!(select_asset(&assets, "linux", "x86_64").is_some());
        /* Wrong arch shouldn't match. */
        assert!(select_asset(&assets, "linux", "aarch64").is_none());
    }

    #[test]
    fn no_asset_when_platform_is_unsupported_or_missing() {
        let assets = vec![asset("hydra-server-windows-x86_64.exe")];
        assert!(select_asset(&assets, "linux", "x86_64").is_none());
        assert!(select_asset(&[], "linux", "x86_64").is_none());
        /* Unknown arch yields no tokens, so nothing matches. */
        assert!(select_asset(&assets, "windows", "mips").is_none());
    }

    #[test]
    fn checksums_and_signatures_are_not_treated_as_binaries() {
        let assets = vec![
            asset("hydra-server-linux-x86_64.sha256"),
            asset("hydra-server-linux-x86_64.asc"),
        ];
        assert!(select_asset(&assets, "linux", "x86_64").is_none());
    }

    #[test]
    fn recognises_container_cgroups() {
        assert!(cgroup_indicates_container("12:cpuset:/docker/abc123def456"));
        assert!(cgroup_indicates_container("0::/kubepods/burstable/pod123"));
        assert!(cgroup_indicates_container("1:name=systemd:/podman/xyz"));
        assert!(!cgroup_indicates_container("0::/init.scope"));
        assert!(!cgroup_indicates_container("12:cpuset:/user.slice"));
    }
}
