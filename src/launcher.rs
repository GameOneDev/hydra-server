//! What the launcher says about itself.
//!
//! Every request the launcher makes carries `User-Agent: Hydra Launcher
//! v3.2.1` — the version is already here, on the hot path, at no cost. This
//! module is the one place that reads it, so the panel and the database agree
//! on what a version string is.
//!
//! The header is written by the client and therefore by anyone who can reach
//! this server, so nothing here trusts its shape: a version is stored only
//! when it looks like one, and it is short enough that no screen showing it
//! has to defend itself.

use axum::http::{header::USER_AGENT, HeaderMap};

/// What the launcher calls itself, and the only client this server records a
/// version for.
const CLIENT: &str = "Hydra Launcher";

/// Long enough for any real version (`3.2.1-beta.4+build.7` is 20), short
/// enough that a hostile header can't fill a column with it.
const MAX_LENGTH: usize = 32;

/// The launcher version behind a request, when it is one and it said.
pub fn version(headers: &HeaderMap) -> Option<String> {
    headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(from_user_agent)
}

/// `Hydra Launcher v3.2.1` -> `3.2.1`.
///
/// Anything else — a browser hitting the portal, curl, a launcher too old to
/// name itself — is `None`, which callers keep meaning "no news", never
/// "downgraded to nothing".
fn from_user_agent(user_agent: &str) -> Option<String> {
    let rest = strip_prefix_ignoring_case(user_agent.trim_start(), CLIENT)?;

    /* `Name vX` is what the launcher sends; `Name/X` is what the rest of the
       world does, and costs one trim to accept. */
    let token = rest
        .trim_start()
        .trim_start_matches('/')
        .split_whitespace()
        .next()?;
    let version = token.strip_prefix(['v', 'V']).unwrap_or(token);

    plausible(version).then(|| version.to_string())
}

/// Whether this could be a version rather than something a client made up.
/// Deliberately narrow: a digit first, and nothing but what versions are
/// spelled with.
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
        assert_eq!(from_user_agent("Hydra Launcher v3.2.1").as_deref(), Some("3.2.1"));
        assert_eq!(
            from_user_agent("Hydra Launcher v3.2.1-beta.4").as_deref(),
            Some("3.2.1-beta.4")
        );
        /* Same client, spelled by a different build or a proxy that rewrote
           the header's casing and spacing. */
        assert_eq!(from_user_agent("  hydra launcher V3.2.1  ").as_deref(), Some("3.2.1"));
        assert_eq!(from_user_agent("Hydra Launcher/3.2.1").as_deref(), Some("3.2.1"));
        /* Electron and friends append their own build details. */
        assert_eq!(
            from_user_agent("Hydra Launcher v3.2.1 (win32; x64)").as_deref(),
            Some("3.2.1")
        );
    }

    #[test]
    fn anything_that_is_not_a_launcher_reports_nothing() {
        assert_eq!(from_user_agent(""), None);
        assert_eq!(from_user_agent("curl/8.4.0"), None);
        assert_eq!(
            from_user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"),
            None
        );
        /* Named itself but not its version. */
        assert_eq!(from_user_agent("Hydra Launcher"), None);
        assert_eq!(from_user_agent("Hydra Launcher v"), None);
    }

    #[test]
    fn a_hostile_header_is_not_a_version() {
        assert_eq!(from_user_agent("Hydra Launcher v<script>alert(1)</script>"), None);
        /* Every query here is parameterised, so this was never an
           injection — but a column the panel prints is no place for it. */
        assert_eq!(from_user_agent("Hydra Launcher v3.2.1'; DROP TABLE users--"), None);
        let too_long = format!("Hydra Launcher v1.{}", "9".repeat(MAX_LENGTH));
        assert_eq!(from_user_agent(&too_long), None);
        /* A multi-byte lead must not panic on the prefix slice. */
        assert_eq!(from_user_agent("Hydra Läuncher v3.2.1"), None);
    }

    #[test]
    fn reads_the_header_off_a_request() {
        let mut headers = HeaderMap::new();
        assert_eq!(version(&headers), None);

        headers.insert(USER_AGENT, "Hydra Launcher v4.0.0".parse().unwrap());
        assert_eq!(version(&headers).as_deref(), Some("4.0.0"));
    }
}
