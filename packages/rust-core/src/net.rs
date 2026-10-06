//! Shared endpoint defaults and loopback classification.

use std::net::IpAddr;

/// Default address of a running `freellama serve` (proxy and control plane).
pub const DEFAULT_SERVE_ENDPOINT: &str = "http://127.0.0.1:11435";
/// Default address of the Ollama server itself.
pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

/// The bracket-free form of a host: `url` renders IPv6 hosts as `[::1]`, while `IpAddr` and
/// `SocketAddr::ip()` do not. Only a matching `[`…`]` pair is removed.
pub(crate) fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

/// True for `localhost`, any `127.0.0.0/8` address, and `::1` (bare or in URL brackets).
/// Unspecified addresses (`0.0.0.0`, `::`), `localhost.` and IPv4-mapped `::ffff:127.0.0.1` are
/// deliberately not loopback.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = strip_ipv6_brackets(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// True for `0.0.0.0` and `::` (bare or in URL brackets): a listen wildcard, not a destination.
pub(crate) fn is_unspecified_host(host: &str) -> bool {
    strip_ipv6_brackets(host)
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_unspecified())
}

/// True when the URL's host is a loopback host; false for unparseable URLs.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(is_loopback_host))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls_include_bracketed_ipv6_and_the_whole_127_block() {
        for url in [
            "http://[::1]:11434",
            "http://localhost:11434",
            "http://LOCALHOST",
            "http://127.0.0.1:11434",
            "http://127.8.9.10",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "http://[::2]:11434",
            "http://10.0.0.1:11434",
            "http://example.com",
            "not a url",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
    }

    #[test]
    fn wildcards_trailing_dot_and_mapped_ipv4_are_not_loopback() {
        for host in [
            "0.0.0.0",
            "::",
            "[::]",
            "localhost.",
            "[::ffff:127.0.0.1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
        for url in [
            "http://0.0.0.0:11434",
            "http://[::]:11434",
            "http://localhost.:11434",
            "http://[::ffff:127.0.0.1]:11434",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    #[test]
    fn brackets_strip_only_as_a_pair_and_unspecified_is_detected() {
        assert_eq!(strip_ipv6_brackets("[::1]"), "::1");
        assert_eq!(strip_ipv6_brackets("::1"), "::1");
        assert_eq!(strip_ipv6_brackets("[::1"), "[::1");
        assert_eq!(strip_ipv6_brackets("example.com"), "example.com");
        for host in ["0.0.0.0", "::", "[::]"] {
            assert!(is_unspecified_host(host), "{host}");
        }
        for host in ["127.0.0.1", "[::1]", "localhost", "example.com"] {
            assert!(!is_unspecified_host(host), "{host}");
        }
    }
}
