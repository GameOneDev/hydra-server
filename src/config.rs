use rand::RngCore;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Config {
    /// Address the HTTP server binds to, e.g. `0.0.0.0:8788`.
    pub bind: String,
    /// Public base URL clients can reach this server on. Used to build the
    /// upload/download URLs handed to the launcher, so it must be the URL
    /// your friends use (reverse-proxy domain, LAN IP, ...).
    pub public_url: String,
    /// Directory for the SQLite database and stored save files.
    pub data_dir: PathBuf,
    /// Official Hydra API used to validate launcher access tokens.
    pub official_api_url: String,
    /// Secret for signing storage URLs and admin sessions. Generated and
    /// persisted under the data dir when not provided.
    pub secret: String,
    /// Password for the /admin panel. Panel is disabled when empty.
    pub admin_password: String,
    /// Max total stored bytes per user (0 = unlimited).
    pub max_bytes_per_user: u64,
    /// Max save backups kept per game per user.
    pub backups_per_game_limit: u32,
    /// Comma-separated official user ids or usernames allowed to use this
    /// server. Empty = everyone with a valid official login.
    pub allowed_users: Vec<String>,
    /// Whether the background update checker runs.
    pub update_check_enabled: bool,
    /// Hours between update checks (a minimum of 1 is enforced on use).
    pub update_check_interval_hours: u64,
    /// `owner/repo` the update checker watches for new server releases.
    pub update_repo: String,
    /// Default for the `auto_update` setting: install detected updates
    /// automatically. Off by default; editable live from the admin panel.
    pub auto_update_enabled: bool,
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Reads a boolean env var. Empty/unset falls back to `default`; otherwise
/// `0`/`false`/`no`/`off` (any case) are false and anything else is true.
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => !matches!(
            value.trim().to_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        _ => default,
    }
}

impl Config {
    pub fn from_env() -> Self {
        let bind = env("HYDRA_SERVER_BIND", "0.0.0.0:8788");
        let data_dir = PathBuf::from(env("HYDRA_SERVER_DATA_DIR", "./data"));

        std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

        let secret = match std::env::var("HYDRA_SERVER_SECRET") {
            Ok(secret) if !secret.trim().is_empty() => secret,
            _ => load_or_generate_secret(&data_dir),
        };

        Self {
            public_url: env("HYDRA_SERVER_PUBLIC_URL", &format!("http://{bind}"))
                .trim_end_matches('/')
                .to_string(),
            bind,
            official_api_url: env(
                "HYDRA_OFFICIAL_API_URL",
                "https://hydra-api-us-east-1.losbroxas.org",
            )
            .trim_end_matches('/')
            .to_string(),
            secret,
            admin_password: env("HYDRA_ADMIN_PASSWORD", ""),
            max_bytes_per_user: env("HYDRA_MAX_BYTES_PER_USER", "0").parse().unwrap_or(0),
            backups_per_game_limit: env("HYDRA_BACKUPS_PER_GAME_LIMIT", "100")
                .parse()
                .unwrap_or(100),
            allowed_users: env("HYDRA_ALLOWED_USERS", "")
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            update_check_enabled: env_bool("HYDRA_UPDATE_CHECK", true),
            update_check_interval_hours: env("HYDRA_UPDATE_CHECK_INTERVAL_HOURS", "6")
                .parse::<u64>()
                .unwrap_or(6)
                .max(1),
            update_repo: env("HYDRA_UPDATE_REPO", "gameonedev/hydra-server")
                .trim()
                .trim_matches('/')
                .to_string(),
            auto_update_enabled: env_bool("HYDRA_AUTO_UPDATE", false),
            data_dir,
        }
    }

    pub fn storage_dir(&self) -> PathBuf {
        self.data_dir.join("storage")
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("hydra-server.db")
    }
}

fn load_or_generate_secret(data_dir: &Path) -> String {
    let secret_path = data_dir.join(".secret");

    if let Ok(existing) = std::fs::read_to_string(&secret_path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = hex::encode(bytes);

    std::fs::write(&secret_path, &secret).expect("failed to persist server secret");
    secret
}
