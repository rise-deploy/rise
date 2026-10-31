//! HTTP clients for the relay's upstream discovery, JWKS and token requests.

use crate::server::ssrf::{self, SsrfConfig};
use oauthrelay_core::ClientAssertionHttpClient;

/// The relay's client for every upstream request: discovery, JWKS and the
/// credential-bearing token requests.
///
/// It is Rise's OAuth token client: 5 s connect and 10 s total timeouts, and no
/// redirects, because the same client carries the upstream client secret and a
/// redirect would forward it to a destination the secret is not bound to. The
/// resolver SSRF-checks every URL the relay can fetch before handing it over.
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
