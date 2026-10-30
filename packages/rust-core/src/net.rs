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

/// Build an HTTP client from `configure`. reqwest 0.13 verifies TLS with the platform verifier,
/// which refuses to build at all when the system has no CA certificates (a slim or distroless
/// container), even for a client that only ever talks plain HTTP to a loopback Ollama
/// (reqwest issue #2988). In that one case the client is rebuilt with an empty trust store:
/// `http://` upstreams keep working, and an `https://` upstream fails its handshake with a
/// certificate error that names the cause, instead of the whole process failing at startup.
/// Bundling static roots instead would trade this for certificates that go stale between builds.
///
/// # Errors
///
/// Returns the builder error for any other misconfiguration.
pub fn build_http_client(
    configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    match configure(reqwest::Client::builder()).build() {
        Err(error) if is_missing_system_roots(&error) => {
            warn_missing_system_roots();
            configure(reqwest::Client::builder())
                .tls_certs_only(Vec::new())
                .build()
        }
        built => built,
    }
}

/// The platform verifier's error for an empty system certificate store.
fn is_missing_system_roots(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .to_string()
            .contains("No CA certificates were loaded from the system")
        {
            return true;
        }
        current = error.source();
    }
    false
}

/// Whether a client in this process fell back to an empty trust store.
pub fn system_roots_missing() -> bool {
    MISSING_ROOTS.load(std::sync::atomic::Ordering::Relaxed)
}

static MISSING_ROOTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn warn_missing_system_roots() {
    if !MISSING_ROOTS.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "freellama: no system CA certificates found; plain-HTTP upstreams work, HTTPS upstreams will fail certificate checks. Install your distribution's ca-certificates package or set SSL_CERT_FILE."
        );
    }
}

#[cfg(test)]
mod tls_root_tests {
    use super::is_missing_system_roots;

    #[derive(Debug)]
    struct Wrapped(Box<dyn std::error::Error + 'static>);
    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("builder error")
        }
    }
    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    #[test]
    fn missing_roots_is_found_anywhere_in_the_error_chain() {
        let inner: Box<dyn std::error::Error> =
            "unexpected error: No CA certificates were loaded from the system".into();
        assert!(is_missing_system_roots(&Wrapped(inner)));
        let other: Box<dyn std::error::Error> = "invalid TLS versions".into();
        assert!(!is_missing_system_roots(&Wrapped(other)));
    }

    #[test]
    fn a_client_builds_on_this_host() {
        super::build_http_client(|builder| builder).expect("client builds");
    }
}
