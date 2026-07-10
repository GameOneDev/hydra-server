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
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
