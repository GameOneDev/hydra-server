//! Working out who is actually calling.
//!
//! Every address this server records — the brute-force counter's key, the
//! `ip` column on sign-in events — is only as good as this. Behind a reverse
//! proxy the socket's peer is the proxy, and behind Cloudflare *and* a reverse
//! proxy it is the proxy's view of Cloudflare, so the real address only exists
//! in a header.
//!
//! Headers are also trivially forged by anyone who can reach the server
//! directly, which is why none of this happens unless
//! `HYDRA_TRUST_PROXY_HEADERS` says there is a proxy in front.

use crate::config::Config;
use axum::http::HeaderMap;
use std::net::{IpAddr, SocketAddr};

/// Set (and overwritten) by Cloudflare on every request it forwards. Unlike
/// `X-Forwarded-For` it cannot carry anything the visitor put there, and
/// unlike `X-Real-IP` it survives a reverse proxy that fills that in with
/// Cloudflare's own edge address.
const CLOUDFLARE: &str = "cf-connecting-ip";
/// Cloudflare Enterprise's equivalent, and what several other CDNs use.
const TRUE_CLIENT: &str = "true-client-ip";
const FORWARDED_FOR: &str = "x-forwarded-for";
const REAL_IP: &str = "x-real-ip";

/// An address and where it was found, so the panel can explain itself.
pub struct Resolved {
    pub ip: String,
    /// Header name, or "peer address" when nothing was trusted or usable.
    pub source: String,
}

/// The caller's address.
pub fn of(config: &Config, headers: &HeaderMap, socket: Option<SocketAddr>) -> String {
    resolve(config, headers, socket).ip
}

pub fn resolve(config: &Config, headers: &HeaderMap, socket: Option<SocketAddr>) -> Resolved {
    let peer = || Resolved {
        ip: socket
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        source: "peer address".to_string(),
    };

    if !config.trust_proxy_headers {
        return peer();
    }

    /* A named header is the whole answer: an operator who says where the
    address lives has ruled out the others, and quietly falling back to
    guessing would reintroduce exactly the header they ruled out. */
    if !config.client_ip_header.is_empty() {
        let header = config.client_ip_header.to_ascii_lowercase();
        return match first_address(headers, &header) {
            Some(ip) => Resolved { ip, source: header },
            None => peer(),
        };
    }

    for header in [CLOUDFLARE, TRUE_CLIENT] {
        if let Some(ip) = first_address(headers, header) {
            return Resolved {
                ip,
                source: header.to_string(),
            };
        }
    }

    if let Some(ip) = forwarded_for(headers, config.trusted_proxy_hops) {
        return Resolved {
            ip,
            source: FORWARDED_FOR.to_string(),
        };
    }

    if let Some(ip) = first_address(headers, REAL_IP) {
        return Resolved {
            ip,
            source: REAL_IP.to_string(),
        };
    }

    peer()
}

/// `X-Forwarded-For`, counted from the right.
///
/// Each proxy appends the address it saw, so the rightmost entries were
/// written by our own infrastructure and the leftmost is whatever the visitor
/// claimed — Cloudflare appends to a header the visitor sent rather than
/// replacing it, so the traditional "take the first entry" is forgeable.
/// `hops` is how many proxies added an entry *after* the one we want: 0 for a
/// single reverse proxy, 1 when Cloudflare sits in front of it.
fn forwarded_for(headers: &HeaderMap, hops: usize) -> Option<String> {
    let chain: Vec<&str> = headers
        .get_all(FORWARDED_FOR)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();

    let index = chain.len().checked_sub(1 + hops)?;
    parse_address(chain[index])
}

fn first_address(headers: &HeaderMap, header: &str) -> Option<String> {
    let value = headers.get(header)?.to_str().ok()?;
    parse_address(value.split(',').next()?)
}

/// Parses one entry into a bare address.
///
/// Anything that isn't an address is dropped rather than passed through: these
/// strings become rate-limit keys and log lines, and a header nobody validated
/// is a free way to spawn a new identity per request.
fn parse_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Some(ip.to_string());
    }

    /* Some proxies include the source port — "203.0.113.7:54321" or
    "[2001:db8::1]:54321" — and a few bracket IPv6 without one. */
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Some(addr.ip().to_string());
    }

    raw.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inner| inner.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
}

/// What the request actually carried, for the panel's proxy diagnostic.
pub fn observed_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    [CLOUDFLARE, TRUE_CLIENT, FORWARDED_FOR, REAL_IP, "forwarded"]
        .iter()
        .filter_map(|header| {
            let joined = headers
                .get_all(*header)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then(|| (header.to_string(), joined))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn config(trust: bool, hops: usize, header: &str) -> Config {
        let mut config = Config::for_test();
        config.trust_proxy_headers = trust;
        config.trusted_proxy_hops = hops;
        config.client_ip_header = header.to_string();
        config
    }

    fn peer() -> Option<SocketAddr> {
        Some("10.0.0.5:44321".parse().unwrap())
    }

    /// The reason this module exists: without the flag every visitor shares
    /// the proxy's address, which is both a useless log line and a shared
    /// lockout.
    #[test]
    fn headers_are_ignored_until_a_proxy_is_declared() {
        let map = headers(&[("x-forwarded-for", "203.0.113.7")]);
        let resolved = resolve(&config(false, 0, ""), &map, peer());

        assert_eq!(resolved.ip, "10.0.0.5");
        assert_eq!(resolved.source, "peer address");
    }

    /// Cloudflare in front of a reverse proxy: `X-Real-IP` is the edge and the
    /// first `X-Forwarded-For` entry is whatever the visitor sent, so neither
    /// may win over the header Cloudflare controls.
    #[test]
    fn cloudflares_header_beats_a_forged_chain() {
        let map = headers(&[
            ("cf-connecting-ip", "203.0.113.7"),
            ("x-forwarded-for", "1.2.3.4, 203.0.113.7, 172.70.9.1"),
            ("x-real-ip", "172.70.9.1"),
        ]);
        let resolved = resolve(&config(true, 0, ""), &map, peer());

        assert_eq!(resolved.ip, "203.0.113.7");
        assert_eq!(resolved.source, "cf-connecting-ip");
    }

    /// Without a CDN header the chain is counted from the right, because that
    /// end was written by machines we run.
    #[test]
    fn forwarded_for_is_counted_from_the_right() {
        let map = headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.7, 172.70.9.1")]);

        assert_eq!(resolve(&config(true, 0, ""), &map, peer()).ip, "172.70.9.1");
        assert_eq!(
            resolve(&config(true, 1, ""), &map, peer()).ip,
            "203.0.113.7"
        );
        assert_eq!(resolve(&config(true, 2, ""), &map, peer()).ip, "1.2.3.4");
        /* Further back than the chain goes is a misconfiguration, not an
        excuse to pick the forgeable end. */
        assert_eq!(resolve(&config(true, 3, ""), &map, peer()).ip, "10.0.0.5");
    }

    #[test]
    fn ports_brackets_and_split_headers_are_understood() {
        let map = headers(&[
            ("x-forwarded-for", "203.0.113.7:54321"),
            ("x-forwarded-for", "[2001:db8::1]:443"),
        ]);
        assert_eq!(
            resolve(&config(true, 1, ""), &map, peer()).ip,
            "203.0.113.7"
        );
        assert_eq!(
            resolve(&config(true, 0, ""), &map, peer()).ip,
            "2001:db8::1"
        );
    }

    /// A header full of junk must not become a rate-limit key.
    #[test]
    fn unparseable_addresses_fall_through() {
        let map = headers(&[("x-forwarded-for", "unknown"), ("x-real-ip", "not-an-ip")]);
        assert_eq!(resolve(&config(true, 0, ""), &map, peer()).ip, "10.0.0.5");
    }

    #[test]
    fn a_named_header_is_the_only_one_consulted() {
        let map = headers(&[
            ("x-client-ip", "203.0.113.7"),
            ("cf-connecting-ip", "198.51.100.2"),
        ]);
        let resolved = resolve(&config(true, 0, "X-Client-IP"), &map, peer());
        assert_eq!(resolved.ip, "203.0.113.7");
        assert_eq!(resolved.source, "x-client-ip");

        /* Named but absent falls back to the socket, never to the headers the
        operator chose against. */
        let other = headers(&[("cf-connecting-ip", "198.51.100.2")]);
        assert_eq!(
            resolve(&config(true, 0, "X-Client-IP"), &other, peer()).ip,
            "10.0.0.5"
        );
    }
}
