use schemars::JsonSchema;
use serde::Deserialize;
use std::net::IpAddr;
use std::time::Duration;

/// SSRF validation configuration.
///
/// Controls how Rise validates URLs before making server-side requests.
/// All fields default to the most restrictive settings (HTTPS required,
/// private networks blocked, no trusted hosts).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SsrfConfig {
    /// Allow private/loopback IPs in SSRF-validated URLs.
    /// WARNING: Only enable for local development. Never enable in production.
    // Accepts a native YAML boolean or a string ("true"/"false") via
    // deserialize_bool_flexible so it can be driven by an env-interpolated config
    // value (e.g. allow_private_networks: "${RISE_SSRF_ALLOW_PRIVATE:-false}"),
    // which the loader resolves to a string before deserialization. Kept as `//`
    // (not `///`) so the generated JSON schema description is unchanged.
    #[serde(default, deserialize_with = "deserialize_bool_flexible")]
    #[schemars(with = "bool")]
    pub allow_private_networks: bool,

    /// Allow HTTP (non-TLS) URLs in SSRF-validated requests.
    /// WARNING: Only enable for local development. Never enable in production.
    #[serde(default, deserialize_with = "deserialize_bool_flexible")]
    #[schemars(with = "bool")]
    pub allow_http: bool,

    /// Hostnames that are allowed to resolve to private/internal IP addresses.
    /// Use this to permit SSRF-validated requests to trusted internal services
    /// (e.g., an internal Keycloak or OIDC provider) without enabling
    /// `allow_private_networks`.
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

impl SsrfConfig {
    /// Check whether a hostname is in the trusted hosts list.
    fn is_trusted_host(&self, host: &str) -> bool {
        self.trusted_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }

    /// Whether HTTP is allowed (either via `allow_http` or `allow_private_networks`).
    fn http_allowed(&self) -> bool {
        self.allow_http || self.allow_private_networks
    }
}

/// Deserialize a boolean that may arrive as a native bool or as a string.
///
/// The settings loader interpolates `${VAR}` inside string config values and
/// produces a JSON string, so an env-driven flag like
/// `allow_private_networks: "${RISE_SSRF_ALLOW_PRIVATE:-false}"` reaches serde as
/// the string `"false"`. serde does not coerce string→bool, so accept both forms
/// (case-insensitive `true`/`false`, plus `1`/`0`/`yes`/`no`/`on`/`off`).
pub(crate) fn deserialize_bool_flexible<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        Str(String),
    }

    match BoolOrString::deserialize(deserializer)? {
        BoolOrString::Bool(b) => Ok(b),
        BoolOrString::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(D::Error::custom(format!(
                "invalid boolean string {other:?}; expected true/false"
            ))),
        },
    }
}

/// Errors that can occur during SSRF-safe URL validation.
#[derive(Debug)]
pub enum SsrfError {
    /// URL must use HTTPS scheme.
    HttpsRequired,
    /// The URL could not be parsed.
    InvalidUrl(String),
    /// The hostname could not be resolved.
    DnsResolutionFailed(String),
    /// The resolved IP address is in a blocked range (private, loopback, link-local, etc.).
    BlockedIpRange(IpAddr),
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsrfError::HttpsRequired => write!(f, "URL must use HTTPS"),
            SsrfError::InvalidUrl(msg) => write!(f, "Invalid URL: {}", msg),
            SsrfError::DnsResolutionFailed(msg) => write!(f, "DNS resolution failed: {}", msg),
            SsrfError::BlockedIpRange(ip) => {
                write!(f, "URL resolves to a blocked IP address: {}", ip)
            }
        }
    }
}

/// Check whether an IP address is in a private, loopback, link-local, or otherwise
/// internal range that should not be reachable from server-side requests.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()             // 127.0.0.0/8
                || ipv4.is_private()        // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || ipv4.is_link_local()     // 169.254.0.0/16 (includes AWS metadata 169.254.169.254)
                || ipv4.is_unspecified()     // 0.0.0.0
                || ipv4.is_broadcast()      // 255.255.255.255
                || ipv4.is_documentation()  // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
                || ipv4.octets()[0] == 100 && (ipv4.octets()[1] & 0xC0) == 64 // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()          // ::1
                || ipv6.is_unspecified() // ::
                // Unique local (fc00::/7)
                || (ipv6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local (fe80::/10)
                || (ipv6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped addresses (::ffff:0:0/96) — check the mapped IPv4
                || match ipv6.to_ipv4_mapped() {
                    Some(mapped_v4) => is_blocked_ip(&IpAddr::V4(mapped_v4)),
                    None => false,
                }
        }
    }
}

/// Validate that a URL is safe to fetch (SSRF protection).
///
/// Checks:
/// 1. Scheme must be HTTPS (unless `allow_http` or `allow_private_networks` is true)
/// 2. Hostname must be present
/// 3. All resolved IP addresses must not be in blocked ranges (unless the hostname
///    is in `trusted_hosts` or `allow_private_networks` is true)
///
/// Trusted hosts are allowed to resolve to private IPs while keeping SSRF
/// protection for all other hostnames. Use this for internal services like
/// Keycloak or other OIDC providers that resolve to cluster-internal addresses.
pub async fn validate_url(url: &str, config: &SsrfConfig) -> Result<(), SsrfError> {
    // Parse the URL
    let parsed = url::Url::parse(url).map_err(|e| SsrfError::InvalidUrl(e.to_string()))?;

    // Require HTTPS (unless relaxed)
    if !config.http_allowed() && parsed.scheme() != "https" {
        return Err(SsrfError::HttpsRequired);
    }

    // Extract hostname
    let host = parsed
        .host_str()
        .ok_or_else(|| SsrfError::InvalidUrl("URL has no host".to_string()))?;

    // Check if the hostname is trusted (allowed to resolve to private IPs)
    let is_trusted = config.is_trusted_host(host);

    // If host is already an IP address, check it directly
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !config.allow_private_networks && !is_trusted && is_blocked_ip(&ip) {
            return Err(SsrfError::BlockedIpRange(ip));
        }
        return Ok(());
    }

    // Skip DNS resolution checks when private networks are allowed or host is trusted
    if config.allow_private_networks || is_trusted {
        return Ok(());
    }

    // Resolve hostname and check all resolved addresses
    let port = parsed.port().unwrap_or(443);
    let addr = format!("{}:{}", host, port);

    let resolved = tokio::net::lookup_host(&addr)
        .await
        .map_err(|e| SsrfError::DnsResolutionFailed(format!("{}: {}", host, e)))?;

    let addrs: Vec<_> = resolved.collect();
    if addrs.is_empty() {
        return Err(SsrfError::DnsResolutionFailed(format!(
            "{}: no addresses resolved",
            host
        )));
    }

    for addr in &addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(SsrfError::BlockedIpRange(addr.ip()));
        }
    }

    Ok(())
}

/// Create an HTTP client configured with SSRF mitigations.
///
/// The client has:
/// - 5-second connect timeout and 10-second total request timeout
/// - Custom redirect policy (max 3 hops, HTTPS-only, blocks private/internal IPs)
///
/// When `allow_private_networks` is true, redirect checks for HTTPS and blocked IPs are skipped.
/// When `allow_http` is true, only the HTTPS requirement on redirects is relaxed.
pub fn safe_client(config: &SsrfConfig) -> reqwest::Client {
    let config = config.clone();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 3 {
                attempt.error("too many redirects")
            } else if !config.http_allowed() && attempt.url().scheme() != "https" {
                attempt.error("redirect target must use HTTPS")
            } else if let Some(host) = attempt.url().host_str() {
                if !config.allow_private_networks && !config.is_trusted_host(host) {
                    if let Ok(ip) = host.parse::<IpAddr>() {
                        if is_blocked_ip(&ip) {
                            return attempt
                                .error(format!("redirect target resolves to blocked IP: {}", ip));
                        }
                    }
                }
                attempt.follow()
            } else {
                attempt.error("redirect target has no host")
            }
        }))
        .build()
        .expect("failed to build SSRF-safe HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict() -> SsrfConfig {
        SsrfConfig::default()
    }

    fn permissive() -> SsrfConfig {
        SsrfConfig {
            allow_private_networks: true,
            allow_http: true,
            trusted_hosts: vec![],
        }
    }

    fn with_allow_http() -> SsrfConfig {
        SsrfConfig {
            allow_http: true,
            ..SsrfConfig::default()
        }
    }

    fn with_trusted(hosts: &[&str]) -> SsrfConfig {
        SsrfConfig {
            trusted_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            ..SsrfConfig::default()
        }
    }

    #[test]
    fn test_blocked_ipv4_loopback() {
        assert!(is_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"127.0.0.2".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv4_private() {
        assert!(is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_blocked_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"192.168.1.100".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv4_link_local() {
        // AWS metadata endpoint
        assert!(is_blocked_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip(&"169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv4_unspecified() {
        assert!(is_blocked_ip(&"0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv4_cgnat() {
        assert!(is_blocked_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn test_allowed_ipv4_public() {
        assert!(!is_blocked_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"142.250.80.46".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv6_loopback() {
        assert!(is_blocked_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv6_unspecified() {
        assert!(is_blocked_ip(&"::".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv6_unique_local() {
        assert!(is_blocked_ip(&"fc00::1".parse().unwrap()));
        assert!(is_blocked_ip(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv6_link_local() {
        assert!(is_blocked_ip(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn test_blocked_ipv4_mapped_ipv6() {
        // ::ffff:127.0.0.1
        assert!(is_blocked_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:169.254.169.254
        assert!(is_blocked_ip(&"::ffff:169.254.169.254".parse().unwrap()));
        // ::ffff:10.0.0.1
        assert!(is_blocked_ip(&"::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_allowed_ipv6_public() {
        assert!(!is_blocked_ip(&"2607:f8b0:4004:800::200e".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_validate_url_rejects_http() {
        let result = validate_url("http://example.com", &strict()).await;
        assert!(matches!(result, Err(SsrfError::HttpsRequired)));
    }

    #[tokio::test]
    async fn test_validate_url_rejects_invalid_url() {
        let result = validate_url("not-a-url", &strict()).await;
        assert!(matches!(result, Err(SsrfError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn test_validate_url_rejects_ip_loopback() {
        let result = validate_url("https://127.0.0.1/path", &strict()).await;
        assert!(matches!(result, Err(SsrfError::BlockedIpRange(_))));
    }

    #[tokio::test]
    async fn test_validate_url_rejects_ip_metadata() {
        let result = validate_url("https://169.254.169.254/latest/meta-data/", &strict()).await;
        assert!(matches!(result, Err(SsrfError::BlockedIpRange(_))));
    }

    #[tokio::test]
    async fn test_validate_url_rejects_private_ip() {
        let result = validate_url("https://10.0.0.1/internal", &strict()).await;
        assert!(matches!(result, Err(SsrfError::BlockedIpRange(_))));

        let result = validate_url("https://192.168.1.1/admin", &strict()).await;
        assert!(matches!(result, Err(SsrfError::BlockedIpRange(_))));
    }

    #[tokio::test]
    async fn test_validate_url_accepts_public_https() {
        // Use a known public IP to avoid DNS dependency and make the test deterministic.
        let result = validate_url("https://8.8.8.8/", &strict()).await;
        assert!(
            result.is_ok(),
            "expected public HTTPS URL to be accepted, got: {:?}",
            result
        );
    }

    #[test]
    fn test_safe_client_builds_successfully() {
        let _client = safe_client(&strict());
    }

    // Tests for allow_private_networks = true

    #[tokio::test]
    async fn test_validate_url_allows_http_when_private_networks_enabled() {
        let result = validate_url("http://localhost:5556", &permissive()).await;
        assert!(
            result.is_ok(),
            "expected HTTP URL to be accepted with allow_private_networks, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_url_allows_loopback_when_private_networks_enabled() {
        let result = validate_url("https://127.0.0.1/path", &permissive()).await;
        assert!(
            result.is_ok(),
            "expected loopback IP to be accepted with allow_private_networks, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_url_allows_private_ip_when_private_networks_enabled() {
        let result = validate_url("https://192.168.1.1/admin", &permissive()).await;
        assert!(
            result.is_ok(),
            "expected private IP to be accepted with allow_private_networks, got: {:?}",
            result
        );
    }

    #[test]
    fn test_safe_client_builds_with_private_networks() {
        let _client = safe_client(&permissive());
    }

    // Tests for trusted_hosts

    #[tokio::test]
    async fn test_validate_url_allows_trusted_host_with_private_ip() {
        // A trusted host should be allowed even if it would resolve to a private IP.
        // We use an IP-literal URL here to test the is_trusted check on the host string.
        let config = with_trusted(&["10.0.0.1"]);
        let result = validate_url("https://10.0.0.1/internal", &config).await;
        assert!(
            result.is_ok(),
            "expected trusted IP-literal host to be accepted, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_url_rejects_untrusted_host_with_private_ip() {
        let config = with_trusted(&["other.internal"]);
        let result = validate_url("https://10.0.0.1/internal", &config).await;
        assert!(
            matches!(result, Err(SsrfError::BlockedIpRange(_))),
            "expected untrusted IP-literal to be blocked, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_url_trusted_host_still_requires_https() {
        let config = with_trusted(&["internal.example.com"]);
        let result = validate_url("http://internal.example.com/api", &config).await;
        assert!(
            matches!(result, Err(SsrfError::HttpsRequired)),
            "expected HTTPS requirement even for trusted hosts, got: {:?}",
            result
        );
    }

    #[test]
    fn test_trusted_host_case_insensitive() {
        let config = with_trusted(&["Keycloak.Example.COM"]);
        assert!(config.is_trusted_host("keycloak.example.com"));
        assert!(config.is_trusted_host("KEYCLOAK.EXAMPLE.COM"));
    }

    // Tests for allow_http

    #[tokio::test]
    async fn test_validate_url_allows_http_when_allow_http_enabled() {
        let result = validate_url("http://example.com", &with_allow_http()).await;
        assert!(
            result.is_ok(),
            "expected HTTP URL to be accepted with allow_http, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_validate_url_allow_http_still_blocks_private_ips() {
        let result = validate_url("https://10.0.0.1/internal", &with_allow_http()).await;
        assert!(
            matches!(result, Err(SsrfError::BlockedIpRange(_))),
            "expected private IP to be blocked even with allow_http, got: {:?}",
            result
        );
    }
}
