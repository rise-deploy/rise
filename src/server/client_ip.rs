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
//! 3. Peer trusted and `X-Forwarded-For` present (on any number of header
//!    lines): walk its entries right to left, skipping addresses that are
//!    themselves trusted proxies. The first untrusted address is the client.
//!    When every entry is trusted, the leftmost entry is the client. The
//!    header's raw bytes are split on `,` and each entry is judged on its own,
//!    so garbage left of an entry a trusted proxy appended never affects it.
//!    Blank entries are skipped. An entry that is not UTF-8 or not an IP
//!    address (after accepting `[v6]`, `ip:port` and `[v6]:port` forms) ends
//!    the walk where it stands: the chain cannot be followed past it, and the
//!    client resolves to the peer. A chain of more than [`MAX_FORWARDED_ENTRIES`]
//!    entries is treated the same way.
//! 4. Peer trusted and no `X-Forwarded-For` header at all: `X-Real-IP` if it
//!    parses as an IP address, otherwise the peer. `X-Real-IP` is never
//!    consulted when `X-Forwarded-For` is present, usable or not: a proxy that
//!    keeps a client-supplied `X-Real-IP` would otherwise let a client choose
//!    its own address by making `X-Forwarded-For` unusable.
//!
//! IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are normalized to IPv4
//! before matching and keying, so one client does not get two rate-limit
//! buckets depending on how the socket reported it.

use std::convert::Infallible;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
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

    /// The full address, if one was established.
    pub fn addr(&self) -> Option<IpAddr> {
        self.0
    }

    /// The key IP-keyed rate limits count this client under.
    ///
    /// IPv4 clients are keyed by address. IPv6 clients are keyed by their /64,
    /// because a single host or subscriber typically controls a whole /64 and
    /// can rotate addresses within it at will. Unknown clients share the
    /// `unknown` key. Use [`Display`](fmt::Display) for the full address (logs,
    /// audit records).
    pub fn rate_limit_key(&self) -> String {
        match self.0 {
            Some(IpAddr::V6(v6)) => {
                let prefix = u128::from(v6) & !((1u128 << 64) - 1);
                format!("{}/64", Ipv6Addr::from(prefix))
            }
            _ => self.to_string(),
        }
    }
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
        let client = if headers.contains_key("x-forwarded-for") {
            self.resolve_forwarded_for(headers)
        } else {
            headers
                .get("x-real-ip")
                .and_then(|v| parse_ip(std::str::from_utf8(v.as_bytes()).ok()?))
        };
        ClientIp(Some(client.unwrap_or(peer)))
    }

    /// Walk `X-Forwarded-For` right to left. `None` when the chain is unusable
    /// (see the module docs).
    fn resolve_forwarded_for(&self, headers: &HeaderMap) -> Option<IpAddr> {
        let mut entries: Vec<&[u8]> = Vec::new();
        for value in headers.get_all("x-forwarded-for") {
            for entry in value.as_bytes().split(|&b| b == b',') {
                if entries.len() == MAX_FORWARDED_ENTRIES {
                    return None;
                }
                entries.push(entry);
            }
        }
        let mut leftmost_trusted = None;
        for entry in entries.into_iter().rev() {
            let entry = entry.trim_ascii();
            if entry.is_empty() {
                continue;
            }
            let ip = parse_ip(std::str::from_utf8(entry).ok()?)?;
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

/// Longest `X-Forwarded-For` chain (entries across all header lines) that is
/// followed. Real chains are a handful of hops; a longer one is treated as
/// unusable rather than walked.
pub const MAX_FORWARDED_ENTRIES: usize = 32;

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
    fn present_xff_never_defers_to_real_ip() {
        // A proxy that keeps a client-supplied X-Real-IP must not let the
        // client pick its address by spoiling X-Forwarded-For.
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", "bogus"), ("x-real-ip", "198.51.100.23")],
        );
        assert_eq!(out, "10.0.2.7");
        let out = resolve(
            "10.0.2.7",
            &[("x-forwarded-for", ""), ("x-real-ip", "198.51.100.23")],
        );
        assert_eq!(out, "10.0.2.7");
        let out = resolve(
            "10.0.2.7",
            &[
                ("x-forwarded-for", "203.0.113.5"),
                ("x-real-ip", "198.51.100.23"),
            ],
        );
        assert_eq!(out, "203.0.113.5");
    }

    #[test]
    fn non_utf8_xff_never_defers_to_real_ip() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_bytes(b"\x80").unwrap());
        h.insert("x-real-ip", HeaderValue::from_static("198.51.100.23"));
        let out = TrustedProxies::default().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "10.0.2.7");
    }

    #[test]
    fn invalid_real_ip_is_the_peer() {
        assert_eq!(
            resolve("10.0.2.7", &[("x-real-ip", "also bogus")]),
            "10.0.2.7"
        );
    }

    #[test]
    fn non_utf8_entry_ends_the_walk_at_that_entry() {
        // The rightmost entry is not text: the chain is broken, so the peer.
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"198.51.100.23, \xff").unwrap(),
        );
        let out = TrustedProxies::default().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "10.0.2.7");
    }

    #[test]
    fn non_utf8_entry_left_of_a_proxy_appended_entry_cannot_poison_it() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"\xff\x80, 198.51.100.23, 10.0.1.5").unwrap(),
        );
        let out = TrustedProxies::default().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "198.51.100.23");
    }

    #[test]
    fn non_utf8_value_on_an_earlier_header_line_is_not_reached() {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", HeaderValue::from_bytes(b"\x80").unwrap());
        h.append(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.23, 10.0.1.5"),
        );
        let out = TrustedProxies::default().resolve(Some("10.0.2.7".parse().unwrap()), &h);
        assert_eq!(out.to_string(), "198.51.100.23");
    }

    #[test]
    fn overlong_chain_resolves_to_the_peer() {
        let at_cap = std::iter::repeat_n("10.0.1.5", MAX_FORWARDED_ENTRIES - 1)
            .chain(["198.51.100.23"])
            .rev()
            .collect::<Vec<_>>()
            .join(", ");
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_str(&at_cap).unwrap());
        let t = TrustedProxies::default();
        let peer = Some("10.0.2.7".parse().unwrap());
        assert_eq!(t.resolve(peer, &h).to_string(), "198.51.100.23");

        let over = format!("6.6.6.6, {at_cap}");
        h.insert("x-forwarded-for", HeaderValue::from_str(&over).unwrap());
        assert_eq!(t.resolve(peer, &h).to_string(), "10.0.2.7");

        // The cap counts entries across header lines.
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", HeaderValue::from_str(&at_cap).unwrap());
        h.append("x-forwarded-for", HeaderValue::from_static("6.6.6.6"));
        assert_eq!(t.resolve(peer, &h).to_string(), "10.0.2.7");
    }

    #[test]
    fn rate_limit_key_groups_ipv6_by_64() {
        let key = |ip: &str| {
            TrustedProxies::default()
                .resolve(Some(ip.parse().unwrap()), &HeaderMap::new())
                .rate_limit_key()
        };
        assert_eq!(key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), "2001:db8:1:2::/64");
        assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(key("203.0.113.9"), "203.0.113.9");
        assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(ClientIp::UNKNOWN.rate_limit_key(), "unknown");
        // Display keeps the full address.
        let full = TrustedProxies::default()
            .resolve(Some("2001:db8:1:2::1".parse().unwrap()), &HeaderMap::new());
        assert_eq!(full.to_string(), "2001:db8:1:2::1");
    }

    #[test]
    fn blank_entries_are_skipped() {
        let out = resolve("10.0.2.7", &[("x-forwarded-for", "198.51.100.23,, ")]);
        assert_eq!(out, "198.51.100.23");
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
