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

/// The checks of [`validate_url`] that need no DNS: the scheme, a host, and the
/// address of an IP-literal host. A host name passes; the connect-time
/// [`SsrfResolver`] filters the addresses it resolves to, so the name is
/// checked against the addresses actually connected to rather than an earlier
/// lookup.
pub fn validate_url_without_dns(url: &url::Url, config: &SsrfConfig) -> Result<(), SsrfError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SsrfError::InvalidUrl(format!(
            "unsupported scheme {}",
            url.scheme()
        )));
    }
    if !config.http_allowed() && url.scheme() != "https" {
        return Err(SsrfError::HttpsRequired);
    }
    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::InvalidUrl("URL has no host".to_string()))?;
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => return Ok(()),
    };
    if !config.allow_private_networks && !config.is_trusted_host(host) && is_blocked_ip(&ip) {
        return Err(SsrfError::BlockedIpRange(ip));
    }
    Ok(())
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
    safe_client_builder(config)
        .build()
        .expect("failed to build SSRF-safe HTTP client")
}

/// Credential-bearing token requests never carry their body to a redirect target.
pub fn token_client(config: &SsrfConfig) -> reqwest::Client {
    token_client_builder(config)
        .build()
        .expect("failed to build OAuth token HTTP client")
}

pub(crate) fn token_client_builder(config: &SsrfConfig) -> reqwest::ClientBuilder {
    safe_client_builder(config).redirect(reqwest::redirect::Policy::none())
}

/// The base of every SSRF-safe client. Besides the redirect policy below, it
/// refuses plain `http` URLs unless `http_allowed()`, and resolves host names
/// through [`SsrfResolver`], so a name that resolves to a blocked address at
/// connect time (DNS rebinding, or a URL that was never passed through
/// [`validate_url`]) is never connected to. IP-literal hosts bypass DNS and are
/// covered by [`validate_url`] only.
pub(crate) fn safe_client_builder(config: &SsrfConfig) -> reqwest::ClientBuilder {
    let resolver = SsrfResolver::new(config.clone());
    let config = config.clone();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .https_only(!config.http_allowed())
        .dns_resolver(resolver)
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
}

/// A DNS resolver that drops blocked addresses (see [`validate_url`]): a name
/// whose addresses are all blocked fails to resolve. Names in `trusted_hosts`,
/// and every name when `allow_private_networks` is on, resolve unfiltered, as
/// does the host of an egress proxy configured through the environment
/// (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`), which usually sits on a private
/// network. Through such a proxy the target's DNS is resolved by the proxy,
/// so only [`validate_url`] applies to the target.
#[derive(Clone, Debug)]
pub struct SsrfResolver {
    config: SsrfConfig,
    proxy_hosts: Vec<String>,
}

impl SsrfResolver {
    pub fn new(config: SsrfConfig) -> Self {
        Self {
            config,
            proxy_hosts: env_proxy_hosts(),
        }
    }

    fn unfiltered(&self, host: &str) -> bool {
        self.config.allow_private_networks
            || self.config.is_trusted_host(host)
            || self
                .proxy_hosts
                .iter()
                .any(|proxy| proxy.eq_ignore_ascii_case(host))
    }
}

/// The hosts of the egress proxies reqwest picks up from the environment.
fn env_proxy_hosts() -> Vec<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .filter_map(|name| std::env::var(name).ok())
    .filter_map(|value| {
        let value = if value.contains("://") {
            value
        } else {
            format!("http://{value}")
        };
        url::Url::parse(&value).ok()?.host_str().map(str::to_owned)
    })
    .collect()
}

impl reqwest::dns::Resolve for SsrfResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let unfiltered = self.unfiltered(&host);
        Box::pin(async move {
            let resolved: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<std::net::SocketAddr> = resolved
                .iter()
                .copied()
                .filter(|addr| unfiltered || !is_blocked_ip(&addr.ip()))
                .collect();
            if allowed.is_empty() {
                let reason = match resolved.first() {
                    Some(addr) => SsrfError::BlockedIpRange(addr.ip()),
                    None => {
                        SsrfError::DnsResolutionFailed(format!("{host}: no addresses resolved"))
                    }
                };
                return Err(Box::new(std::io::Error::other(reason.to_string()))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
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

    async fn resolve(config: SsrfConfig, host: &str) -> Result<Vec<IpAddr>, String> {
        use reqwest::dns::Resolve;
        SsrfResolver::new(config)
            .resolve(host.parse().unwrap())
            .await
            .map(|addrs| addrs.map(|addr| addr.ip()).collect())
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn resolver_drops_blocked_addresses_unless_trusted() {
        let error = resolve(with_allow_http(), "localhost").await.unwrap_err();
        assert!(error.contains("blocked"), "{error}");
        let addrs = resolve(with_trusted(&["LOCALHOST"]), "localhost")
            .await
            .unwrap();
        assert!(addrs.iter().all(|ip| ip.is_loopback()), "{addrs:?}");
        assert!(!resolve(permissive(), "localhost").await.unwrap().is_empty());
    }

    /// A listener that counts the connections it accepts.
    async fn counting_listener() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
            }
        });
        (port, accepted)
    }

    use std::sync::Arc;

    #[tokio::test]
    async fn clients_never_connect_to_blocked_or_plain_http_targets() {
        use std::sync::atomic::Ordering;
        let (port, accepted) = counting_listener().await;
        let url = format!("http://localhost:{port}/");

        // A name resolving to a blocked address, without validate_url.
        for client in [
            safe_client(&with_allow_http()),
            token_client(&with_allow_http()),
        ] {
            assert!(client.get(&url).send().await.is_err());
        }
        // Plain http when it is not allowed.
        assert!(safe_client(&with_trusted(&["localhost"]))
            .get(&url)
            .send()
            .await
            .is_err());
        assert_eq!(accepted.load(Ordering::SeqCst), 0);

        // A trusted host is reached.
        let trusted = SsrfConfig {
            allow_http: true,
            trusted_hosts: vec!["localhost".into()],
            ..SsrfConfig::default()
        };
        assert!(safe_client(&trusted)
            .get(&url)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dns_free_validation_checks_scheme_and_address_literals() {
        let check = |url: &str, config: &SsrfConfig| {
            validate_url_without_dns(&url::Url::parse(url).unwrap(), config).is_ok()
        };
        assert!(check("https://idp.example.com/jwks", &strict()));
        assert!(check("https://93.184.215.14/jwks", &strict()));
        for url in [
            "http://idp.example.com/jwks",
            "ftp://idp.example.com/jwks",
            "https://169.254.169.254/latest",
            "https://10.0.0.1/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://0.0.0.0/",
        ] {
            assert!(!check(url, &strict()), "{url}");
        }
        assert!(check("http://idp.example.com/", &with_allow_http()));
        assert!(!check("http://10.0.0.1/", &with_allow_http()));
        assert!(check("https://10.0.0.1/", &with_trusted(&["10.0.0.1"])));
        assert!(check("http://10.0.0.1/", &permissive()));
    }
}
