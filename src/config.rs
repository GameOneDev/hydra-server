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
    /// Whether the server deletes a save it has replaced: the older cloud
    /// save a commit supersedes, the older emulation save in a slot. Set to
    /// `false` to keep both and delete by hand.
    pub auto_delete_saves: bool,
    /// Comma-separated official user ids or usernames allowed to use this
    /// server. Empty = everyone with a valid official login.
    pub allowed_users: Vec<String>,
    /// Failed sign-ins from one address before it is locked out.
    pub login_max_attempts: u32,
    /// How long a locked-out address stays locked.
    pub login_lockout_minutes: i64,
    /// Trust proxy headers for the client address. Only enable behind a proxy
    /// you control — otherwise a client can spoof its own address and walk
    /// straight past the login lockout.
    pub trust_proxy_headers: bool,
    /// Header carrying the client address, when the default order (Cloudflare,
    /// then `X-Forwarded-For`, then `X-Real-IP`) isn't what your proxy sets.
    /// Empty = use the default order.
    pub client_ip_header: String,
    /// How many proxies append to `X-Forwarded-For` after the entry we want.
    /// 0 for a single reverse proxy; 1 with Cloudflare in front of it.
    pub trusted_proxy_hops: usize,
    /// Bearer token guarding `/metrics`. Empty = the endpoint is open.
    pub metrics_token: String,
    /// Set to `false` to switch off `/metrics` entirely.
    pub metrics_enabled: bool,
    /// Hours between automatic database backups (0 = only manual ones).
    pub backup_interval_hours: u64,
    /// How many automatic backups to keep before pruning the oldest.
    pub backup_keep: usize,
    /// Days of event history to keep. Older rows are pruned daily.
    pub event_retention_days: i64,
    /// Quiet minutes after which a launcher counts as away, so its next call
    /// is logged as coming online. 0 switches the presence log off.
    pub presence_idle_minutes: i64,
    /// Set to `false` to switch off the user-facing portal at `/portal`.
    pub portal_enabled: bool,
    /// Path on the official API that exchanges credentials for tokens. The
    /// portal posts the sign-in form there so a user never has to find their
    /// own access token.
    pub official_login_path: String,
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Anything but an explicit "false"/"0"/"no" counts as on, so a typo in a
/// compose file can't silently disable a feature the operator asked for.
fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        ),
        Err(_) => default,
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
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
            auto_delete_saves: env_flag("HYDRA_AUTO_DELETE_SAVES", true),
            allowed_users: env("HYDRA_ALLOWED_USERS", "")
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            login_max_attempts: env_parse("HYDRA_LOGIN_MAX_ATTEMPTS", 8),
            login_lockout_minutes: env_parse("HYDRA_LOGIN_LOCKOUT_MINUTES", 15),
            trust_proxy_headers: env_flag("HYDRA_TRUST_PROXY_HEADERS", false),
            client_ip_header: env("HYDRA_CLIENT_IP_HEADER", "")
                .trim()
                .to_ascii_lowercase(),
            trusted_proxy_hops: env_parse("HYDRA_TRUSTED_PROXY_HOPS", 0),
            metrics_token: env("HYDRA_METRICS_TOKEN", ""),
            metrics_enabled: env_flag("HYDRA_METRICS_ENABLED", true),
            backup_interval_hours: env_parse("HYDRA_BACKUP_INTERVAL_HOURS", 24),
            backup_keep: env_parse("HYDRA_BACKUP_KEEP", 7),
            event_retention_days: env_parse("HYDRA_EVENT_RETENTION_DAYS", 90),
            presence_idle_minutes: env_parse("HYDRA_PRESENCE_IDLE_MINUTES", 15),
            portal_enabled: env_flag("HYDRA_PORTAL_ENABLED", true),
            official_login_path: env("HYDRA_OFFICIAL_LOGIN_PATH", "/auth/login"),
            data_dir,
        }
    }

    /// Defaults without reading the environment, so a unit test can set the
    /// one field it is about without a process-wide `set_var`.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            bind: "127.0.0.1:8788".to_string(),
            public_url: "http://127.0.0.1:8788".to_string(),
            data_dir: PathBuf::from("./data"),
            official_api_url: String::new(),
            secret: "test-secret".to_string(),
            admin_password: String::new(),
            max_bytes_per_user: 0,
            backups_per_game_limit: 100,
            auto_delete_saves: true,
            allowed_users: Vec::new(),
            login_max_attempts: 8,
            login_lockout_minutes: 15,
            trust_proxy_headers: false,
            client_ip_header: String::new(),
            trusted_proxy_hops: 0,
            metrics_token: String::new(),
            metrics_enabled: true,
            backup_interval_hours: 24,
            backup_keep: 7,
            event_retention_days: 90,
            presence_idle_minutes: 15,
            portal_enabled: true,
            official_login_path: "/auth/login".to_string(),
        }
    }

    /// Where automatic and manual database backups are written.
    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
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

/// The host part of an address as written in a URL authority or a `Host`
/// header: no port, no brackets around an IPv6 literal.
fn hostname(authority: &str) -> &str {
    let authority = authority.trim();

    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }

    authority.split(':').next().unwrap_or(authority)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Why `public_url` cannot be the address a launcher uploads to, if it can't.
///
/// Every presigned upload and every image link is built from this URL, and the
/// launcher connects to exactly what it is given. The default, taken from the
/// bind address, is `http://0.0.0.0:8788`: right for the server's own socket,
/// but an address nobody can connect to. A server left like that hands out a
/// slot that no launcher can fill, and nothing on the server side ever sees a
/// request to fail.
///
/// `request_host` is the `Host` the caller reached the server by, when there
/// is a caller. A loopback `public_url` is fine for a server used from the
/// machine it runs on, and only a problem once somebody else reaches it.
pub fn public_url_problem(public_url: &str, request_host: Option<&str>) -> Option<String> {
    let Ok(url) = reqwest::Url::parse(public_url) else {
        return Some(format!(
            "{public_url:?} is not a valid URL. Set HYDRA_SERVER_PUBLIC_URL to the address \
             people use to reach this server, for example http://192.168.1.10:8788 or \
             https://hydra.example.com"
        ));
    };

    if !matches!(url.scheme(), "http" | "https") {
        return Some(format!(
            "{public_url:?} must start with http:// or https://. Set HYDRA_SERVER_PUBLIC_URL \
             to the address people use to reach this server"
        ));
    }

    let host = url.host_str().map(hostname).unwrap_or_default();
    let ip = host.parse::<std::net::IpAddr>().ok();

    if host.is_empty() || ip.is_some_and(|ip| ip.is_unspecified()) {
        return Some(format!(
            "{public_url} is not an address a launcher can connect to ({host} means \"every \
             network interface\" to the server, not a place to send requests). This is what \
             HYDRA_SERVER_PUBLIC_URL defaults to when it is not set: set it to the address \
             people use to reach this server, for example http://192.168.1.10:8788 or \
             https://hydra.example.com"
        ));
    }

    let reached_as = request_host.map(hostname).filter(|host| !host.is_empty());
    if is_loopback_host(host) && reached_as.is_some_and(|reached| !is_loopback_host(reached)) {
        return Some(format!(
            "{public_url} only works on the machine the server runs on, but this request \
             arrived as {}. Set HYDRA_SERVER_PUBLIC_URL to the address other machines use \
             to reach this server",
            reached_as.unwrap_or_default()
        ));
    }

    None
}

#[cfg(test)]
mod public_url_tests {
    use super::*;

    #[test]
    fn the_default_derived_from_the_bind_address_is_flagged() {
        let problem = public_url_problem("http://0.0.0.0:8788", None).expect("a problem");

        assert!(problem.contains("HYDRA_SERVER_PUBLIC_URL"));
        assert!(problem.contains("0.0.0.0"));
        assert!(public_url_problem("http://[::]:8788", None).is_some());
    }

    #[test]
    fn a_url_without_a_scheme_or_host_is_flagged() {
        assert!(public_url_problem("192.168.1.10:8788", None).is_some());
        assert!(public_url_problem("hydra.example.com", None).is_some());
        assert!(public_url_problem("ftp://hydra.example.com", None).is_some());
        assert!(public_url_problem("", None).is_some());
    }

    #[test]
    fn an_address_people_can_use_is_not() {
        for url in [
            "http://192.168.1.10:8788",
            "https://hydra.example.com",
            "http://hydra.lan",
            "http://[2001:db8::1]:8788",
        ] {
            assert_eq!(public_url_problem(url, None), None, "{url}");
            assert_eq!(public_url_problem(url, Some("192.168.1.10:8788")), None);
        }
    }

    #[test]
    fn loopback_is_fine_until_someone_else_connects() {
        for url in [
            "http://127.0.0.1:8788",
            "http://localhost:8788",
            "http://[::1]:8788",
        ] {
            assert_eq!(public_url_problem(url, None), None, "{url}");
            assert_eq!(public_url_problem(url, Some("localhost:8788")), None);
            assert_eq!(public_url_problem(url, Some("127.0.0.1")), None);
            assert_eq!(public_url_problem(url, Some("[::1]:8788")), None);
            assert_eq!(public_url_problem(url, Some("")), None);

            let problem = public_url_problem(url, Some("192.168.1.10:8788")).expect("a problem");
            assert!(problem.contains("192.168.1.10"), "{problem}");
        }
    }

    #[test]
    fn a_host_is_read_without_its_port_or_brackets() {
        assert_eq!(hostname("example.com:443"), "example.com");
        assert_eq!(hostname("example.com"), "example.com");
        assert_eq!(hostname("[::1]:8788"), "::1");
        assert_eq!(hostname(" 10.0.0.1:80 "), "10.0.0.1");
    }
}
