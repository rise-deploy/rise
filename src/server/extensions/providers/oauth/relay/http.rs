//! HTTP clients for the relay's upstream discovery, JWKS and token requests.

use crate::server::ssrf::{self, SsrfConfig};
use oauthrelay_core::{ClientAssertionHttpClient, OutboundDenied, OutboundPolicy, OutboundPurpose};
use url::Url;

/// The relay's client for every upstream request: discovery, JWKS and the
/// credential-bearing token requests.
///
/// It is Rise's OAuth token client: 5 s connect and 10 s total timeouts, and no
/// redirects, because the same client carries the upstream client secret and a
/// redirect would forward it to a destination the secret is not bound to. No
/// redirects also means [`SsrfOutboundPolicy`], which sees only the first URL,
/// sees every URL. The client refuses plain http unless allowed and filters
/// blocked addresses when it resolves a host name.
pub fn relay_http_client(ssrf: &SsrfConfig) -> reqwest::Client {
    ssrf::token_client(ssrf)
}

/// The client for `private_key_jwt` client-assertion metadata. Rise relays
/// never use that client authentication, so it only needs Rise's timeouts and
/// redirect-free policy.
pub fn client_assertion_http_client(
    ssrf: &SsrfConfig,
) -> Result<ClientAssertionHttpClient, reqwest::Error> {
    ClientAssertionHttpClient::new(ssrf::token_client_builder(ssrf))
}

/// The relay's [`OutboundPolicy`], checked before every request it sends:
/// https unless `server.ssrf.allow_http`, and no private, loopback or
/// link-local address literal unless `allow_private_networks` or a
/// `trusted_hosts` entry allows it. It covers the URLs the relay learns from
/// upstream documents, such as a discovered `jwks_uri`, including IP-literal
/// hosts, which no resolver sees. Host names are left to the clients'
/// connect-time [`ssrf::SsrfResolver`], so a name is judged by the address it
/// is connected to; the policy resolves nothing itself.
pub struct SsrfOutboundPolicy {
    ssrf: SsrfConfig,
}

impl SsrfOutboundPolicy {
    pub fn new(ssrf: SsrfConfig) -> Self {
        Self { ssrf }
    }
}

#[async_trait::async_trait]
impl OutboundPolicy for SsrfOutboundPolicy {
    async fn check(&self, url: &Url, purpose: OutboundPurpose) -> Result<(), OutboundDenied> {
        ssrf::validate_url_without_dns(url, &self.ssrf).map_err(|e| {
            tracing::warn!(%purpose, url = %url, "OAuth relay request refused: {e}");
            OutboundDenied::new(format!("{purpose} request to {url} refused: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn allowed(ssrf: &SsrfConfig, url: &str) -> bool {
        SsrfOutboundPolicy::new(ssrf.clone())
            .check(&url.parse().unwrap(), OutboundPurpose::Jwks)
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn the_outbound_policy_follows_the_ssrf_settings() {
        let strict = SsrfConfig::default();
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.1/jwks",
            "https://127.0.0.1/jwks",
            "https://[::1]/jwks",
            "http://93.184.215.14/jwks",
        ] {
            assert!(!allowed(&strict, url).await, "{url}");
        }
        assert!(allowed(&strict, "https://93.184.215.14/jwks").await);
        // Names are judged by the connect-time resolver.
        assert!(allowed(&strict, "https://localhost/jwks").await);

        let trusted = SsrfConfig {
            trusted_hosts: vec!["10.0.0.1".into()],
            ..Default::default()
        };
        assert!(allowed(&trusted, "https://10.0.0.1/jwks").await);
        assert!(!allowed(&trusted, "http://10.0.0.1/jwks").await);
        assert!(!allowed(&trusted, "https://10.0.0.2/jwks").await);

        let http = SsrfConfig {
            allow_http: true,
            ..Default::default()
        };
        assert!(allowed(&http, "http://93.184.215.14/jwks").await);
        assert!(!allowed(&http, "http://10.0.0.1/jwks").await);

        let private = SsrfConfig {
            allow_private_networks: true,
            ..Default::default()
        };
        assert!(allowed(&private, "http://10.0.0.1/jwks").await);
    }
}
