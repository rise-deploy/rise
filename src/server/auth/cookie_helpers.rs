use axum::http::HeaderMap;

/// Cookie name for Rise-issued JWT tokens (used for both UI and ingress auth)
pub const RISE_JWT_COOKIE_NAME: &str = "rise_jwt";

/// Parse cookies from a Cookie header value
///
/// This implements RFC 6265 cookie parsing:
/// - Cookies are separated by semicolons
/// - Leading/trailing whitespace is trimmed
/// - Cookie format is "name=value"
fn parse_cookies(cookie_header: &str) -> impl Iterator<Item = (&str, &str)> {
    cookie_header.split(';').filter_map(|cookie| {
        let cookie = cookie.trim();
        cookie.split_once('=')
    })
}

/// Settings for session cookies
#[derive(Debug, Clone)]
pub struct CookieSettings {
    pub secure: bool,
    /// Optional domain used to expire stale domain-scoped cookies left over from before
    /// the host-only cookie migration. When set, cookie-setting responses also include a
    /// `Max-Age=0` Set-Cookie header with this Domain attribute to clear any stale cookies
    /// that browsers may still be sending.
    pub cookie_domain: Option<String>,
}

/// Create a Rise JWT cookie with the given Rise-issued JWT token
///
/// This cookie is used for both UI authentication and ingress authentication.
/// The JWT's `aud` claim determines its scope (Rise UI or specific project).
///
/// The cookie is configured with:
/// - HttpOnly: Prevents JavaScript access (XSS protection)
/// - Secure: HTTPS-only transmission (configurable for development)
/// - SameSite=Lax: CSRF protection while allowing navigation
/// - Max-Age: Matches JWT expiry time
/// - Path=/: Valid for all paths
///
/// No `Domain` attribute is set — the browser scopes the cookie to the
/// exact host that set it, preventing cross-domain leakage.
pub fn create_rise_jwt_cookie(
    jwt: &str,
    settings: &CookieSettings,
    max_age_seconds: u64,
) -> String {
    let mut cookie_parts = vec![
        format!("{}={}", RISE_JWT_COOKIE_NAME, jwt),
        format!("Max-Age={}", max_age_seconds),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];

    // Only set Secure flag if configured (false for HTTP development)
    if settings.secure {
        cookie_parts.push("Secure".to_string());
    }

    cookie_parts.join("; ")
}

/// Extract the Rise JWT cookie value from request headers
///
/// Uses proper cookie parsing per RFC 6265.
pub fn extract_rise_jwt_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get("cookie")?.to_str().ok()?;

    parse_cookies(cookie_header)
        .find(|(name, _)| *name == RISE_JWT_COOKIE_NAME)
        .map(|(_, value)| value.to_string())
}

/// Create a cookie that expires a legacy domain-scoped `rise_jwt` cookie.
///
/// Browsers treat host-only and domain-scoped cookies as distinct entries (cookie identity
/// includes the domain), so switching from `Domain=.example.com` to a host-only cookie does
/// not automatically remove the old entry. This function produces a `Max-Age=0` cookie with
/// the given `Domain` attribute so the browser expires the stale cookie on the next response.
pub fn clear_legacy_domain_cookie(domain: &str, settings: &CookieSettings) -> String {
    let mut cookie_parts = vec![
        format!("{}=", RISE_JWT_COOKIE_NAME),
        "Max-Age=0".to_string(),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
        format!("Domain={}", domain),
    ];

    if settings.secure {
        cookie_parts.push("Secure".to_string());
    }

    cookie_parts.join("; ")
}

/// Create a cookie that clears the Rise JWT
///
/// Sets Max-Age=0 to immediately expire the cookie
#[allow(dead_code)]
pub fn clear_rise_jwt_cookie(settings: &CookieSettings) -> String {
    let mut cookie_parts = vec![
        format!("{}=", RISE_JWT_COOKIE_NAME),
        "Max-Age=0".to_string(),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];

    if settings.secure {
        cookie_parts.push("Secure".to_string());
    }

    cookie_parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_create_rise_jwt_cookie() {
        let settings = CookieSettings {
            secure: true,
            cookie_domain: None,
        };

        let cookie = create_rise_jwt_cookie("jwt_token_xyz", &settings, 3600);

        assert!(cookie.contains("rise_jwt=jwt_token_xyz"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("Domain="));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn test_extract_rise_jwt_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_static("rise_jwt=my_jwt; other_cookie=value"),
        );

        let jwt = extract_rise_jwt_cookie(&headers);
        assert_eq!(jwt, Some("my_jwt".to_string()));
    }

    #[test]
    fn test_extract_rise_jwt_cookie_not_present() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_static("other_cookie=value"));

        let jwt = extract_rise_jwt_cookie(&headers);
        assert_eq!(jwt, None);
    }

    #[test]
    fn test_clear_rise_jwt_cookie() {
        let settings = CookieSettings {
            secure: true,
            cookie_domain: None,
        };

        let cookie = clear_rise_jwt_cookie(&settings);

        assert!(cookie.contains("rise_jwt="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
        assert!(!cookie.contains("Domain="));
        assert!(cookie.contains("Secure"));
    }
}
