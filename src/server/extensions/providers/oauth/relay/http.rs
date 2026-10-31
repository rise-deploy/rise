//! HTTP clients for the relay's upstream discovery, JWKS and token requests.

use crate::server::ssrf::{self, SsrfConfig};
use oauthrelay_core::{
    ClientAssertionHttpClient, OutboundDenied, OutboundPolicy, OutboundRequest, UpstreamHttpClient,
};
use url::Url;

/// The relay's client for every upstream request: discovery, JWKS and the
/// credential-bearing token requests.
///
/// It is built like Rise's OAuth token client: 5 s connect and 10 s total
/// timeouts, plain http refused unless `server.ssrf` allows it, and host names
/// resolved through [`ssrf::SsrfResolver`], which drops blocked addresses when
/// connecting. oauthrelay never follows redirects with it, so
/// [`SsrfOutboundPolicy`] sees every URL that is requested, and the upstream
/// client secret goes only to the token endpoint it is bound to.
pub fn relay_http_client(ssrf: &SsrfConfig) -> Result<UpstreamHttpClient, reqwest::Error> {
    UpstreamHttpClient::new(ssrf::token_client_builder(ssrf))
}

/// The client for `private_key_jwt` client-assertion metadata, built the same
/// way. Rise relays never use that client authentication.
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

impl SsrfOutboundPolicy {
    /// Whether `url` may be requested; resolves nothing.
    fn allows(&self, url: &Url) -> Result<(), ssrf::SsrfError> {
        ssrf::validate_url_without_dns(url, &self.ssrf)
    }
}

#[async_trait::async_trait]
impl OutboundPolicy for SsrfOutboundPolicy {
    async fn check(&self, request: &OutboundRequest<'_>) -> Result<(), OutboundDenied> {
        self.allows(request.url).map_err(|e| {
            // Only the origin: paths and queries may carry secrets.
            let origin = request.url.origin().ascii_serialization();
            tracing::warn!(
                purpose = %request.purpose,
                relay = ?request.relay,
                %origin,
                "OAuth relay request refused: {e}"
            );
            OutboundDenied::new(format!(
                "{} request to {origin} refused: {e}",
                request.purpose
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn allowed(ssrf: &SsrfConfig, url: &str) -> bool {
        SsrfOutboundPolicy::new(ssrf.clone())
            .allows(&url.parse().unwrap())
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
