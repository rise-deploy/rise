//! Shared URL matching primitives for authentication redirects.
//!
//! These helpers operate on parsed URL components. Callers must never use raw
//! string prefix matching for redirect targets: userinfo, lookalike hosts, and
//! host suffixes can all make a URL string appear to start with a trusted URL
//! while resolving to a different origin.

use url::Url;

fn normalized_host(host: &str) -> &str {
    host.trim_end_matches('.')
}

/// Returns whether `candidate` is `base` or a dot-anchored subdomain of it.
///
/// DNS hostnames are case-insensitive and a trailing root dot is semantically
/// equivalent, so both are ignored for matching.
pub(crate) fn host_is_same_or_subdomain(candidate: &str, base: &str) -> bool {
    let candidate = normalized_host(candidate);
    let base = normalized_host(base);

    if candidate.is_empty() || base.is_empty() {
        return false;
    }

    candidate.eq_ignore_ascii_case(base)
        || (candidate.len() > base.len()
            && candidate
                .get(candidate.len() - base.len()..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(base))
            && candidate.as_bytes()[candidate.len() - base.len() - 1] == b'.')
}

/// Returns whether two URLs have the same HTTP(S) origin.
///
/// Paths, queries, and fragments are deliberately ignored. Explicit default
/// ports compare equal to their implicit equivalents (`:443` for HTTPS and
/// `:80` for HTTP).
pub(crate) fn origins_match(candidate: &Url, allowed: &Url) -> bool {
    if !matches!(candidate.scheme(), "http" | "https") || candidate.scheme() != allowed.scheme() {
        return false;
    }

    let (Some(candidate_host), Some(allowed_host)) = (candidate.host_str(), allowed.host_str())
    else {
        return false;
    };

    host_is_same_or_subdomain(candidate_host, allowed_host)
        && host_is_same_or_subdomain(allowed_host, candidate_host)
        && candidate.port_or_known_default() == allowed.port_or_known_default()
}

/// Returns whether `path` is a same-origin relative path a browser resolves
/// against the current host.
///
/// Browsers treat `\` as `/` in special-scheme URLs and drop tabs and
/// newlines, so `/\evil.com` and `/<TAB>/evil.com` both reach `evil.com` as a
/// protocol-relative URL. The path must start with a single `/`, contain no
/// backslash, whitespace, or control character, and keep that shape after
/// percent-decoding, so a hop that decodes the value cannot turn it into an
/// off-site target either.
pub(crate) fn is_safe_relative_path(path: &str) -> bool {
    fn rooted_once(path: &str) -> bool {
        path.starts_with('/') && !matches!(path.as_bytes().get(1), Some(b'/' | b'\\'))
    }

    if !rooted_once(path)
        || path
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return false;
    }

    // Decoding stops once a pass leaves the value unchanged; the bound keeps a
    // deeply nested encoding from being accepted unexamined.
    let mut current = path.to_string();
    for _ in 0..4 {
        let Ok(decoded) = urlencoding::decode(&current) else {
            return false;
        };
        if decoded == current {
            return true;
        }
        if !rooted_once(&decoded) || decoded.chars().any(|c| c == '\\' || c.is_control()) {
            return false;
        }
        current = decoded.into_owned();
    }
    false
}

/// Returns whether a URL host is a loopback name or address.
///
/// The entire `.localhost` namespace is reserved for loopback use by RFC 6761.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = normalized_host(host);

    host.eq_ignore_ascii_case("localhost")
        || host
            .to_ascii_lowercase()
            .strip_suffix(".localhost")
            .is_some_and(|prefix| !prefix.is_empty())
        || host == "127.0.0.1"
        || matches!(host, "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matching_is_case_insensitive_dot_anchored_and_trailing_dot_agnostic() {
        assert!(host_is_same_or_subdomain("b.com", "b.com"));
        assert!(host_is_same_or_subdomain("APP.B.COM", "b.com"));
        assert!(host_is_same_or_subdomain("app.b.com.", "B.COM."));

        assert!(!host_is_same_or_subdomain("bb.com", "b.com"));
        assert!(!host_is_same_or_subdomain("b.com.evil.com", "b.com"));
        assert!(!host_is_same_or_subdomain("notb.com", "b.com"));
    }

    #[test]
    fn origins_require_http_scheme_host_and_effective_port_equality() {
        let allowed = Url::parse("https://victim.example.com/callback").unwrap();

        assert!(origins_match(
            &Url::parse("https://victim.example.com/elsewhere?x=1").unwrap(),
            &allowed
        ));
        assert!(origins_match(
            &Url::parse("https://victim.example.com:443/cb").unwrap(),
            &allowed
        ));
        assert!(origins_match(
            &Url::parse("https://victim.example.com./cb").unwrap(),
            &allowed
        ));

        assert!(!origins_match(
            &Url::parse("http://victim.example.com/cb").unwrap(),
            &allowed
        ));
        assert!(!origins_match(
            &Url::parse("https://victim.example.com:444/cb").unwrap(),
            &allowed
        ));
        assert!(!origins_match(
            &Url::parse("https://sub.victim.example.com/cb").unwrap(),
            &allowed
        ));
        assert!(!origins_match(
            &Url::parse("javascript://victim.example.com/cb").unwrap(),
            &allowed
        ));
    }

    #[test]
    fn relative_paths_must_stay_on_the_current_host() {
        for path in [
            "/",
            "/dashboard",
            "/app/project/123?tab=logs#top",
            "/search?q=a%20b",
            "/.rise/auth/signin",
        ] {
            assert!(is_safe_relative_path(path), "expected {path:?} to be safe");
        }

        for path in [
            "",
            "dashboard",
            "//evil.com",
            "/\\evil.com",
            "/\\/evil.com",
            "\\\\evil.com",
            "/foo\\bar",
            "/\t/evil.com",
            "/\n/evil.com",
            "/ /evil.com",
            "/%2fevil.com",
            "/%2F%2Fevil.com",
            "/%5cevil.com",
            "/%5Cevil.com",
            "/%252f%252fevil.com",
            "/a%5cb",
            "/a%0d%0aSet-Cookie:x=y",
            "/%ff",
            "https://evil.com",
            "javascript:alert(1)",
        ] {
            assert!(
                !is_safe_relative_path(path),
                "expected {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn loopback_matching_covers_reserved_names_and_addresses() {
        for host in [
            "localhost",
            "LOCALHOST.",
            "rise.localhost",
            "app.rise.localhost.",
            "127.0.0.1",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "expected {host} to be loopback");
        }

        for host in ["localhost.evil.com", "notlocalhost", "127.0.0.2", "::2"] {
            assert!(
                !is_loopback_host(host),
                "expected {host} not to be loopback"
            );
        }
    }
}
