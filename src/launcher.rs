//! The launcher's version, read off the `User-Agent` it sends every request:
//! `Hydra Launcher v3.2.1`.

use axum::http::{header::USER_AGENT, HeaderMap};

const CLIENT: &str = "Hydra Launcher";
const MAX_LENGTH: usize = 32;

pub fn version(headers: &HeaderMap) -> Option<String> {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(from_user_agent)
}

/// `Hydra Launcher v3.2.1` -> `3.2.1`. Anything else — a browser, curl, a
/// launcher too old to say — is `None`, which callers read as no news.
fn from_user_agent(user_agent: &str) -> Option<String> {
    let rest = strip_prefix_ignoring_case(user_agent.trim_start(), CLIENT)?;
    let token = rest
        .trim_start()
        .trim_start_matches('/')
        .split_whitespace()
        .next()?;
    let version = token.strip_prefix(['v', 'V']).unwrap_or(token);

    plausible(version).then(|| version.to_string())
}

/// The header is written by anyone who can reach this server, so only
/// something shaped like a version reaches a column the panel prints.
fn plausible(version: &str) -> bool {
    version.len() <= MAX_LENGTH
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

fn strip_prefix_ignoring_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_the_launcher_sends() {
        for agent in [
            "Hydra Launcher v3.2.1",
            "  hydra launcher V3.2.1  ",
            "Hydra Launcher/3.2.1",
            "Hydra Launcher v3.2.1 (win32; x64)",
        ] {
            assert_eq!(from_user_agent(agent).as_deref(), Some("3.2.1"), "{agent}");
        }

        assert_eq!(
            from_user_agent("Hydra Launcher v3.2.1-beta.4").as_deref(),
            Some("3.2.1-beta.4")
        );
    }

    #[test]
    fn anything_that_is_not_a_launcher_reports_nothing() {
        for agent in [
            "",
            "curl/8.4.0",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
            "Hydra Launcher",
            "Hydra Launcher v",
        ] {
            assert_eq!(from_user_agent(agent), None, "{agent}");
        }
    }

    #[test]
    fn a_hostile_header_is_not_a_version() {
        let too_long = format!("Hydra Launcher v1.{}", "9".repeat(MAX_LENGTH));

        for agent in [
            "Hydra Launcher v<script>alert(1)</script>",
            "Hydra Launcher v3.2.1'; DROP TABLE users--",
            &too_long,
            /* A multi-byte lead must not panic on the prefix slice. */
            "Hydra Läuncher v3.2.1",
        ] {
            assert_eq!(from_user_agent(agent), None, "{agent}");
        }
    }

    #[test]
    fn reads_the_header_off_a_request() {
        let mut headers = HeaderMap::new();
        assert_eq!(version(&headers), None);

        headers.insert(USER_AGENT, "Hydra Launcher v4.0.0".parse().unwrap());
        assert_eq!(version(&headers).as_deref(), Some("4.0.0"));
    }
}
