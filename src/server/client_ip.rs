//! Client IP resolution for rate limiting and audit.
//!
//! Forwarded-address headers (`X-Forwarded-For`, `X-Real-IP`) are plain
//! request headers: any caller can send them. They are therefore honoured only
//! when the TCP peer that delivered the request is a configured trusted proxy
//! (`server.trusted_proxies`). [`client_ip_middleware`] resolves the address
//! once per request and stores it as a [`ClientIp`] extension; handlers read it
//! through the [`ClientIp`] extractor.
//!
//! Resolution, in order:
//!
//! 1. No peer address (the request did not come through a socket, e.g. a test
//!    driving the router directly): [`ClientIp::UNKNOWN`]. Headers are never
//!    trusted without a peer to vouch for them.
//! 2. Peer not trusted: the peer. Forwarded headers are ignored.
//! 3. Peer trusted: walk `X-Forwarded-For` right to left, skipping addresses
//!    that are themselves trusted proxies. The first untrusted address is the
//!    client. When every entry is trusted, the leftmost entry is the client.
//!    Blank entries are skipped. An entry that is not an IP address (after
//!    accepting `[v6]`, `ip:port` and `[v6]:port` forms) ends the walk without
//!    a result, because the chain cannot be followed past it.
//! 4. No usable `X-Forwarded-For`: `X-Real-IP` if it parses as an IP address.
//! 5. Otherwise the peer itself.
//!
//! IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are normalized to IPv4
//! before matching and keying, so one client does not get two rate-limit
//! buckets depending on how the socket reported it.

use std::convert::Infallible;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{request::Parts, HeaderMap};
use axum::middleware::Next;
use axum::response::Response;
use ipnet::IpNet;
use serde::{Deserialize, Deserializer};

/// The resolved client address of a request, or unknown.
///
/// Displays as the IP address, or `unknown`, which is also the rate-limit key
/// for requests without a resolvable address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(Option<IpAddr>);

impl ClientIp {
    /// No address could be established.
    pub const UNKNOWN: ClientIp = ClientIp(None);
}

impl fmt::Display for ClientIp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(ip) => write!(f, "{ip}"),
            None => f.write_str("unknown"),
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    /// Reads the value [`client_ip_middleware`] stored. A router without the
    /// middleware yields [`ClientIp::UNKNOWN`] rather than failing the request.
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ClientIp>()
            .copied()
            .unwrap_or(ClientIp::UNKNOWN))
    }
}

/// The set of networks whose forwarded-address headers are believed
/// (`server.trusted_proxies`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    /// Trust no one: forwarded headers are never honoured.
    #[cfg(test)]
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// Parse CIDRs. A bare address is a single-host network (`/32` or `/128`).
    pub fn parse<I, S>(entries: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        entries
            .into_iter()
            .map(|entry| {
                let entry = entry.as_ref().trim();
                entry
                    .parse::<IpNet>()
                    .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                    .map_err(|_| format!("`{entry}` is not a CIDR network or IP address"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// Loopback, RFC 1918 and IPv6 unique-local ranges: the addresses a
    /// reverse proxy co-located with Rise (ingress controller, sidecar, load
    /// balancer in the same VPC) connects from.
    pub fn private_networks() -> Self {
        Self::parse([
            "127.0.0.0/8",
            "::1/128",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "fc00::/7",
        ])
        .expect("default trusted proxy networks are valid CIDRs")
    }

    fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(&ip))
    }

    /// Resolve the client address for a request delivered by `peer`.
    pub fn resolve(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> ClientIp {
        let Some(peer) = peer.map(|ip| ip.to_canonical()) else {
            return ClientIp::UNKNOWN;
        };
        if !self.contains(peer) {
            return ClientIp(Some(peer));
        }
        let forwarded = self
            .resolve_forwarded_for(headers)
            .or_else(|| parse_ip(header_text(headers, "x-real-ip")?.as_str()));
        ClientIp(Some(forwarded.unwrap_or(peer)))
    }

    fn resolve_forwarded_for(&self, headers: &HeaderMap) -> Option<IpAddr> {
        let joined = header_text(headers, "x-forwarded-for")?;
        let mut leftmost_trusted = None;
        for entry in joined.rsplit(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let ip = parse_ip(entry)?;
            if !self.contains(ip) {
                return Some(ip);
            }
            leftmost_trusted = Some(ip);
        }
        leftmost_trusted
    }
}

impl Default for TrustedProxies {
    fn default() -> Self {
        Self::private_networks()
    }
}

impl<'de> Deserialize<'de> for TrustedProxies {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let entries = Vec::<String>::deserialize(deserializer)?;
        Self::parse(entries).map_err(serde::de::Error::custom)
    }
}

/// All values of a header joined with commas (a header may be repeated, and a
/// repeated `X-Forwarded-For` is one list). `None` when absent or not text.
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let values = headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().ok())
        .collect::<Option<Vec<_>>>()?;
    (!values.is_empty()).then(|| values.join(","))
}

/// Strictly parse one forwarded address, normalizing IPv4-mapped IPv6.
fn parse_ip(text: &str) -> Option<IpAddr> {
    let text = text.trim();
    let ip = text
        .parse::<IpAddr>()
        .ok()
        .or_else(|| text.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .or_else(|| {
            text.strip_prefix('[')?
                .strip_suffix(']')?
                .parse::<IpAddr>()
                .ok()
        })?;
    Some(ip.to_canonical())
}

/// Resolve the client address of every request and store it as a [`ClientIp`]
/// extension. The peer comes from axum's `ConnectInfo`, so the listener must
/// be served with `into_make_service_with_connect_info::<SocketAddr>()`.
pub async fn client_ip_middleware(
    State(trusted): State<Arc<TrustedProxies>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let client_ip = trusted.resolve(peer, request.headers());
    request.extensions_mut().insert(client_ip);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    fn resolve(peer: &str, pairs: &[(&'static str, &'static str)]) -> String {
        TrustedProxies::default()
            .resolve(Some(peer.parse().unwrap()), &headers(pairs))
            .to_string()
    }

    #[test]
    fn untrusted_peer_ignores_spoofed_headers() {
        let out = resolve(
            "203.0.113.9",
            &[
                ("x-forwarded-for", "1.2.3.4, 5.6.7.8"),
                ("x-real-ip", "9.9.9.9"),
            ],
        );
        assert_eq!(out, "203.0.113.9");
    }

    #[test]
    fn untrusted_peer_gets_one_address_whatever_it_sends() {
        let a = resolve("203.0.113.9", &[("x-real-ip", "1.1.1.1")]);
        let b = resolve("203.0.113.9", &[("x-real-ip", "2.2.2.2")]);
        assert_eq!(a, b);
    }

    #[test]
    fn nginx_ingress_single_xff() {
        let out = resolve("10.42.0.17", &[("x-forwarded-for", "198.51.100.23")]);
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn nginx_ingress_ignores_client_prepended_entries() {
        // The client claimed 1.2.3.4; nginx appended the address it saw.
        let out = resolve(
            "10.42.0.17",
            &[("x-forwarded-for", "1.2.3.4, 198.51.100.23")],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn alb_traefik_rise_chain() {
        // ALB appended the client, Traefik appended the ALB's address.
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "198.51.100.23, 10.0.1.5")],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn alb_chain_with_spoofed_leftmost_entry() {
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "6.6.6.6, 198.51.100.23, 10.0.1.5")],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn all_trusted_chain_uses_leftmost() {
        let out = resolve("10.0.2.7", &[("x-forwarded-for", "192.168.1.5, 10.0.1.5")]);
        assert_eq!(out, "192.168.1.5");
    }

    #[test]
    fn repeated_xff_headers_form_one_list() {
        let out = resolve(
            "10.0.2.7",
            &[
                ("x-forwarded-for", "6.6.6.6"),
                ("x-forwarded-for", "198.51.100.23, 10.0.1.5"),
            ],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn trusted_peer_without_xff_uses_real_ip() {
        let out = resolve("10.42.0.17", &[("x-real-ip", "198.51.100.23")]);
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn xff_wins_over_real_ip() {
        let out = resolve(
            "10.42.0.17",
            &[
                ("x-forwarded-for", "198.51.100.23"),
                ("x-real-ip", "7.7.7.7"),
            ],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn trusted_peer_without_headers_is_the_peer() {
        assert_eq!(resolve("10.42.0.17", &[]), "10.42.0.17");
    }

    #[test]
    fn malformed_entry_ends_walk_and_falls_back() {
        // The rightmost entry is garbage: the chain cannot be followed, so the
        // peer is the client rather than a value the garbage hid behind.
        let out = resolve("10.0.2.7", &[("x-forwarded-for", "6.6.6.6, not-an-ip")]);
        assert_eq!(out, "10.0.2.7");
    }

    #[test]
    fn malformed_entry_left_of_an_untrusted_address_is_never_reached() {
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "garbage, 198.51.100.23, 10.0.1.5")],
        );
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn malformed_xff_falls_back_to_real_ip_then_peer() {
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "bogus"), ("x-real-ip", "198.51.100.23")],
        );
        assert_eq!(out, "198.51.100.23");
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "bogus"), ("x-real-ip", "also bogus")],
        );
        assert_eq!(out, "10.0.2.7");
    }

    #[test]
    fn blank_entries_are_skipped() {
        let out = resolve("10.0.2.7", &[("x-forwarded-for", "198.51.100.23,, ")]);
        assert_eq!(out, "198.51.100.23");
    }

    #[test]
    fn non_text_header_is_unusable() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
        );
        let out = TrustedProxies::default().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "10.0.2.7");
    }

    #[test]
    fn ports_and_brackets_are_tolerated() {
        assert_eq!(
            resolve("10.0.2.7", &[("x-forwarded-for", "198.51.100.23:51234")]),
            "198.51.100.23"
        );
        assert_eq!(
            resolve("10.0.2.7", &[("x-forwarded-for", "[2001:db8::1]")]),
            "2001:db8::1"
        );
        assert_eq!(
            resolve("10.0.2.7", &[("x-forwarded-for", "[2001:db8::1]:443")]),
            "2001:db8::1"
        );
    }

    #[test]
    fn ipv6_client_behind_ipv4_proxy() {
        let out = resolve("10.0.2.7", &[("x-forwarded-for", "2001:db8::42, 10.0.1.5")]);
        assert_eq!(out, "2001:db8::42");
    }

    #[test]
    fn ipv6_untrusted_peer_is_the_peer() {
        let out = resolve("2001:db8::9", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(out, "2001:db8::9");
    }

    #[test]
    fn ipv6_unique_local_and_loopback_peers_are_trusted() {
        assert_eq!(
            resolve("fd00::5", &[("x-forwarded-for", "2001:db8::42")]),
            "2001:db8::42"
        );
        assert_eq!(
            resolve("::1", &[("x-forwarded-for", "198.51.100.23")]),
            "198.51.100.23"
        );
    }

    #[test]
    fn ipv6_all_trusted_chain_uses_leftmost() {
        let out = resolve("fd00::5", &[("x-forwarded-for", "fd00::9, fd00::7")]);
        assert_eq!(out, "fd00::9");
    }

    #[test]
    fn ipv4_mapped_addresses_are_normalized() {
        // Mapped peer is matched against the IPv4 trusted range...
        assert_eq!(
            resolve("::ffff:10.0.2.7", &[("x-forwarded-for", "198.51.100.23")]),
            "198.51.100.23"
        );
        // ...a mapped client is keyed as plain IPv4...
        assert_eq!(
            resolve(
                "10.0.2.7",
                &[("x-forwarded-for", "::ffff:198.51.100.23, 10.0.1.5")]
            ),
            "198.51.100.23"
        );
        // ...and a mapped trusted hop is skipped like its IPv4 form.
        assert_eq!(
            resolve(
                "10.0.2.7",
                &[("x-forwarded-for", "198.51.100.23, ::ffff:10.0.1.5")]
            ),
            "198.51.100.23"
        );
        // An untrusted mapped peer is reported as IPv4.
        assert_eq!(
            resolve("::ffff:203.0.113.9", &[("x-real-ip", "1.1.1.1")]),
            "203.0.113.9"
        );
    }

    #[test]
    fn missing_peer_never_trusts_headers() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4"), ("x-real-ip", "5.6.7.8")]);
        let out = TrustedProxies::default().resolve(None, &h);
        assert_eq!(out, ClientIp::UNKNOWN);
        assert_eq!(out.to_string(), "unknown");
    }

    #[test]
    fn empty_trusted_set_trusts_nobody() {
        let h = headers(&[("x-forwarded-for", "1.2.3.4")]);
        let out = TrustedProxies::none().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "10.0.2.7");
    }

    #[test]
    fn parse_accepts_cidrs_and_bare_addresses() {
        let t = TrustedProxies::parse(["203.0.113.0/24", "198.51.100.7", "2001:db8::/32", "::1"])
            .unwrap();
        assert!(t.contains("203.0.113.200".parse().unwrap()));
        assert!(t.contains("198.51.100.7".parse().unwrap()));
        assert!(!t.contains("198.51.100.8".parse().unwrap()));
        assert!(t.contains("2001:db8::1".parse().unwrap()));
        assert!(t.contains("::1".parse().unwrap()));
    }

    #[test]
    fn parse_rejects_garbage() {
        let err = TrustedProxies::parse(["10.0.0.0/8", "nope"]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn deserializes_from_a_string_list() {
        let t: TrustedProxies = serde_json::from_str(r#"["100.64.0.0/10"]"#).unwrap();
        assert!(t.contains("100.64.1.1".parse().unwrap()));
        assert!(!t.contains("10.0.0.1".parse().unwrap()));
        assert!(serde_json::from_str::<TrustedProxies>(r#"["x"]"#).is_err());
    }

    #[test]
    fn default_networks_are_loopback_private_and_unique_local() {
        let t = TrustedProxies::default();
        for ip in [
            "127.0.0.1",
            "::1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "fd12::1",
        ] {
            assert!(t.contains(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["172.32.0.1", "100.64.0.1", "8.8.8.8", "2001:db8::1"] {
            assert!(!t.contains(ip.parse().unwrap()), "{ip}");
        }
    }

    mod middleware {
        use super::*;
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        fn app(trusted: TrustedProxies) -> Router {
            Router::new()
                .route("/", get(|ip: ClientIp| async move { ip.to_string() }))
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(trusted),
                    client_ip_middleware,
                ))
        }

        async fn body(response: Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        fn request(peer: Option<&str>, xff: &str) -> Request {
            let mut request = Request::builder()
                .uri("/")
                .header("x-forwarded-for", xff)
                .body(Body::empty())
                .unwrap();
            if let Some(peer) = peer {
                request
                    .extensions_mut()
                    .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 4000)));
            }
            request
        }

        #[tokio::test]
        async fn resolves_from_connect_info() {
            let response = app(TrustedProxies::default())
                .oneshot(request(Some("10.0.2.7"), "198.51.100.23, 10.0.1.5"))
                .await
                .unwrap();
            assert_eq!(body(response).await, "198.51.100.23");

            let response = app(TrustedProxies::default())
                .oneshot(request(Some("203.0.113.9"), "1.2.3.4"))
                .await
                .unwrap();
            assert_eq!(body(response).await, "203.0.113.9");
        }

        #[tokio::test]
        async fn without_connect_info_the_client_is_unknown() {
            let response = app(TrustedProxies::default())
                .oneshot(request(None, "1.2.3.4"))
                .await
                .unwrap();
            assert_eq!(body(response).await, "unknown");
        }

        #[tokio::test]
        async fn extractor_without_middleware_is_unknown() {
            let router =
                Router::new().route("/", get(|ip: ClientIp| async move { ip.to_string() }));
            let response = router
                .oneshot(request(Some("10.0.2.7"), "1.2.3.4"))
                .await
                .unwrap();
            assert_eq!(body(response).await, "unknown");
        }
    }
}
