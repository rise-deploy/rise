use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::Api;
use tokio::sync::Mutex;
use tracing::warn;

const DEFAULT_TTL: Duration = Duration::from_secs(15);
pub const DEFAULT_LABEL_SELECTOR: &str = "app.kubernetes.io/name=metacontroller-operator";

pub struct MetacontrollerIpValidator {
    kube_client: kube::Client,
    pub namespace: String,
    pub label_selector: String,
    ttl: Duration,
    cache: Mutex<IpCache>,
}

struct IpCache {
    ips: HashSet<IpAddr>,
    fetched_at: Option<Instant>,
    /// Set on every refresh attempt (success or failure) to rate-limit retries.
    last_attempted_at: Option<Instant>,
}

impl MetacontrollerIpValidator {
    pub fn new(
        kube_client: kube::Client,
        namespace: String,
        label_selector: Option<String>,
    ) -> Self {
        Self {
            kube_client,
            namespace,
            label_selector: label_selector.unwrap_or_else(|| DEFAULT_LABEL_SELECTOR.to_string()),
            ttl: DEFAULT_TTL,
            cache: Mutex::new(IpCache {
                ips: HashSet::new(),
                fetched_at: None,
                last_attempted_at: None,
            }),
        }
    }

    pub async fn validate(&self, addr: SocketAddr) -> Result<(), (StatusCode, &'static str)> {
        let mut cache = self.cache.lock().await;

        let is_fresh = cache
            .fetched_at
            .map(|t| t.elapsed() < self.ttl)
            .unwrap_or(false);
        // Rate-limit: don't retry within TTL of the last attempt (success or failure).
        // Without this, every request would hammer the K8s API during an outage.
        let recently_attempted = cache
            .last_attempted_at
            .map(|t| t.elapsed() < self.ttl)
            .unwrap_or(false);

        if !is_fresh && !recently_attempted {
            cache.last_attempted_at = Some(Instant::now());
            match self.fetch_pod_ips().await {
                Ok(ips) => {
                    cache.ips = ips;
                    cache.fetched_at = Some(Instant::now());
                }
                Err(e) => {
                    warn!("Failed to refresh metacontroller pod IPs: {:?}", e);
                }
            }
        }

        // No successful fetch yet → fail closed (covers both fresh failure and rate-limited cold start)
        if cache.fetched_at.is_none() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Cannot validate source IP: Kubernetes API unavailable",
            ));
        }

        check_ip(&cache.ips, addr)
    }

    async fn fetch_pod_ips(&self) -> anyhow::Result<HashSet<IpAddr>> {
        let pods: Api<Pod> = Api::namespaced(self.kube_client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&self.label_selector);
        let pod_list = pods.list(&lp).await?;

        Ok(pod_list
            .items
            .iter()
            .filter_map(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|s| s.pod_ip.as_deref())
                    .and_then(|ip| ip.parse().ok())
            })
            .collect())
    }
}

/// Normalise an IP to IPv4 when it is an IPv4-mapped IPv6 address (::ffff:x.x.x.x).
/// Kubernetes pod IPs are always stored as plain IPv4 strings, but a dual-stack or
/// IPv6-only listener may present the source address in mapped form.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

fn check_ip(ips: &HashSet<IpAddr>, addr: SocketAddr) -> Result<(), (StatusCode, &'static str)> {
    if ips.contains(&canonical_ip(addr.ip())) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "Webhook source IP not authorized"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    fn addr(a: u8, b: u8, c: u8, d: u8) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), 12345))
    }

    fn ip_set(octets: &[(u8, u8, u8, u8)]) -> HashSet<IpAddr> {
        octets
            .iter()
            .map(|(a, b, c, d)| IpAddr::V4(Ipv4Addr::new(*a, *b, *c, *d)))
            .collect()
    }

    #[test]
    fn test_check_ip_authorized() {
        let ips = ip_set(&[(10, 0, 0, 1)]);
        assert!(check_ip(&ips, addr(10, 0, 0, 1)).is_ok());
    }

    #[test]
    fn test_check_ip_unauthorized() {
        let ips = ip_set(&[(10, 0, 0, 1)]);
        let (status, _) = check_ip(&ips, addr(10, 0, 0, 2)).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_check_ip_empty_set_rejects() {
        let (status, _) = check_ip(&HashSet::new(), addr(10, 0, 0, 1)).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_check_ip_multiple_pods() {
        let ips = ip_set(&[(10, 0, 0, 1), (10, 0, 0, 2), (10, 0, 0, 3)]);
        assert!(check_ip(&ips, addr(10, 0, 0, 2)).is_ok());
        let (status, _) = check_ip(&ips, addr(10, 0, 0, 4)).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_check_ip_ipv4_mapped_ipv6_accepted() {
        // Dual-stack / IPv6 listeners present IPv4 source addresses as ::ffff:x.x.x.x.
        // Pod IPs from the Kubernetes API are plain IPv4 — the two representations must match.
        let ips = ip_set(&[(10, 0, 0, 1)]);
        let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001); // ::ffff:10.0.0.1
        let mapped_addr = SocketAddr::V6(SocketAddrV6::new(mapped, 12345, 0, 0));
        assert!(check_ip(&ips, mapped_addr).is_ok());
    }
}
