//! Minimal async client for Traefik's HTTP API, used by the Docker reconciler's
//! cutover path to learn whether a container's server is actually in Traefik's
//! load-balancer rotation.
//!
//! Every deployment of a group registers its replica containers as servers of
//! ONE shared Traefik load-balancer service
//! (`{service}@docker`, where `{service}` is the group-scoped
//! `sanitize_router_name({project}-{group}-{container})`). When a `health_check`
//! is configured, Traefik runs a per-server health check and exposes the result
//! as a top-level `serverStatus` map of server URL
//! (`http://{ip}:{port}`) → `"UP"`/`"DOWN"` (sibling of `loadBalancer` in the
//! v3.x service payload). Reading this lets the reconciler
//! retire the prior active deployment only once the NEW servers are confirmed
//! serving (UP), so there is no cutover gap.
//!
//! Every failure mode here (unreachable, non-200, parse error, health check not
//! configured so `serverStatus` is absent) maps to `None` so callers can fall
//! back to Rise's own in-process probe.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

/// Timeout for a single Traefik API call. Kept short so a slow/unreachable
/// Traefik can't stall a reconcile tick — on timeout we just fall back.
const TRAEFIK_API_TIMEOUT_SECS: u64 = 3;

/// Async client over Traefik's HTTP API.
///
/// Holds a base URL (with any embedded basic-auth userinfo stripped into
/// separate credentials) and a dedicated `reqwest::Client` with a short timeout.
#[derive(Clone)]
pub struct TraefikApiClient {
    /// Traefik provider suffix appended to the service name when querying
    /// (`{service}@{provider}`). `"docker"` for the Docker provider, `"ecs"` for
    /// the ECS provider — querying the wrong one 404s, so `serverStatus` is never
    /// observed and a health-checked deployment never becomes Healthy.
    provider: &'static str,
    http: reqwest::Client,
    /// Base URL with userinfo removed, no trailing slash (e.g.
    /// `http://rise-traefik:8080`).
    base_url: String,
    /// Basic-auth credentials extracted from the configured URL's userinfo, if
    /// any (`(username, password)`).
    basic_auth: Option<(String, String)>,
}

impl TraefikApiClient {
    /// Build a client from the configured `traefik_api_url`. Any basic-auth
    /// userinfo embedded in the URL is extracted and applied to each request;
    /// the stored base URL has it stripped. Returns `None` when the URL can't be
    /// parsed (the reconciler then has no in-rotation signal — health-checked
    /// containers can't become Healthy).
    /// `provider` is the Traefik provider name the workloads are discovered
    /// through (`"docker"` / `"ecs"`); it becomes the `@provider` suffix on every
    /// service lookup.
    pub fn new(traefik_api_url: &str, provider: &'static str) -> Option<Self> {
        let (base_url, basic_auth) = split_userinfo(traefik_api_url)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TRAEFIK_API_TIMEOUT_SECS))
            .build()
            .ok()?;
        Some(Self {
            http,
            provider,
            base_url,
            basic_auth,
        })
    }

    /// GET a Traefik service's `serverStatus`, returning a map of
    /// server URL (`http://{ip}:{port}`) → `true` (UP) / `false` (DOWN).
    ///
    /// `service` is the bare service base name (e.g. `myapp-default-app`); the
    /// `@{provider}` suffix is appended here. Returns `None` on ANY error
    /// (unreachable, non-200, body parse failure, or `serverStatus` absent —
    /// which is the case when no health check is configured), so the caller
    /// treats the container as not-yet-in-rotation.
    pub async fn server_status(&self, service: &str) -> Option<HashMap<String, bool>> {
        let url = format!(
            "{}/api/http/services/{}@{}",
            self.base_url, service, self.provider
        );
        let mut req = self.http.get(&url);
        if let Some((user, pass)) = &self.basic_auth {
            req = req.basic_auth(user, Some(pass));
        }
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(service = %service, "Traefik API request failed: {e:?}");
                return None;
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(
                service = %service,
                status = resp.status().as_u16(),
                "Traefik API returned non-success"
            );
            return None;
        }
        let body = match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                tracing::debug!(service = %service, "Traefik API body read failed: {e:?}");
                return None;
            }
        };
        parse_server_status(&body)
    }
}

/// Split an optional basic-auth userinfo out of `raw`, returning the base URL
/// (userinfo stripped, no trailing slash) and the credentials. Returns `None`
/// when the URL can't be parsed.
fn split_userinfo(raw: &str) -> Option<(String, Option<(String, String)>)> {
    let mut parsed = url::Url::parse(raw).ok()?;
    let basic_auth = {
        let user = parsed.username();
        if user.is_empty() {
            None
        } else {
            let pass = parsed.password().unwrap_or("").to_string();
            let user = urlencoding::decode(user)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| user.to_string());
            let pass = urlencoding::decode(&pass)
                .map(|c| c.into_owned())
                .unwrap_or(pass);
            Some((user, pass))
        }
    };
    // Strip userinfo from the stored base URL. `set_username`/`set_password`
    // return Err only for cannot-be-a-base URLs, which `http(s)` never is.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    let base = parsed.as_str().trim_end_matches('/').to_string();
    Some((base, basic_auth))
}

/// Shape of a Traefik `/api/http/services/{name}` response we care about: the
/// `serverStatus` map. All other fields are ignored.
///
/// In **Traefik v3.x** (verified against v3.7.1, the version the reference stack
/// pins) `serverStatus` is a **top-level** field of the service object, a sibling
/// of `loadBalancer` — NOT nested inside it. We read the top-level field and,
/// for tolerance across versions, fall back to a `loadBalancer.serverStatus`
/// nesting if a version ever emits it there. When the service has no health
/// check configured, `serverStatus` is absent (Traefik omits it) and both read
/// as `None`.
#[derive(Debug, Deserialize)]
struct ServicePayload {
    #[serde(rename = "serverStatus")]
    server_status: Option<HashMap<String, String>>,
    #[serde(rename = "loadBalancer")]
    load_balancer: Option<LoadBalancerPayload>,
}

#[derive(Debug, Deserialize)]
struct LoadBalancerPayload {
    #[serde(rename = "serverStatus")]
    server_status: Option<HashMap<String, String>>,
}

/// Parse a Traefik service JSON payload into a `server URL → UP?` map.
///
/// Prefers the top-level `serverStatus` (Traefik v3.x); falls back to a
/// `loadBalancer.serverStatus` nesting for version tolerance. Returns `None` when
/// the body doesn't parse, or when `serverStatus` is absent in both places (no
/// health check configured) — meaning "no rotation signal available, fall back".
/// A present-but-empty `serverStatus` yields `Some(empty map)`. Status strings
/// are matched case-insensitively against `"UP"`.
fn parse_server_status(body: &str) -> Option<HashMap<String, bool>> {
    let payload: ServicePayload = serde_json::from_str(body).ok()?;
    let status = payload
        .server_status
        .or_else(|| payload.load_balancer.and_then(|lb| lb.server_status))?;
    Some(
        status
            .into_iter()
            .map(|(server, state)| (server, state.eq_ignore_ascii_case("UP")))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_and_down_server_status() {
        // The REAL Traefik v3.7.1 `/api/http/services/{name}@docker` shape:
        // `serverStatus` is a TOP-LEVEL field of the service object — a sibling
        // of `loadBalancer`, NOT nested inside it. (Getting this wrong silently
        // disabled the whole serverStatus gate; keep this fixture matching real
        // Traefik output.)
        let body = r#"{
            "loadBalancer": {
                "servers": [
                    { "url": "http://10.0.0.2:8080" },
                    { "url": "http://10.0.0.3:8080" }
                ],
                "healthCheck": { "path": "/healthz", "interval": "10s" },
                "passHostHeader": true
            },
            "status": "enabled",
            "usedBy": ["myapp-default-app@docker"],
            "name": "myapp-default-app@docker",
            "provider": "docker",
            "type": "loadbalancer",
            "serverStatus": {
                "http://10.0.0.2:8080": "UP",
                "http://10.0.0.3:8080": "DOWN"
            }
        }"#;
        let map = parse_server_status(body).expect("top-level serverStatus present");
        assert_eq!(map.get("http://10.0.0.2:8080"), Some(&true));
        assert_eq!(map.get("http://10.0.0.3:8080"), Some(&false));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parses_nested_loadbalancer_server_status_fallback() {
        // Version tolerance: if a Traefik version ever nests serverStatus under
        // loadBalancer, we still read it.
        let body = r#"{
            "loadBalancer": {
                "serverStatus": { "http://10.0.0.2:8080": "UP" }
            },
            "name": "x@docker"
        }"#;
        let map = parse_server_status(body).expect("nested serverStatus present");
        assert_eq!(map.get("http://10.0.0.2:8080"), Some(&true));
    }

    #[test]
    fn missing_server_status_yields_none() {
        // No health check configured -> Traefik omits `serverStatus`.
        let body = r#"{
            "loadBalancer": {
                "servers": [ { "url": "http://10.0.0.2:8080" } ],
                "passHostHeader": true
            },
            "status": "enabled",
            "name": "myapp-default-app@docker",
            "provider": "docker"
        }"#;
        assert!(
            parse_server_status(body).is_none(),
            "absent serverStatus must fall back to None"
        );
    }

    #[test]
    fn missing_load_balancer_yields_none() {
        let body = r#"{ "status": "enabled", "name": "x@docker" }"#;
        assert!(parse_server_status(body).is_none());
    }

    #[test]
    fn malformed_body_yields_none() {
        assert!(parse_server_status("not json").is_none());
    }

    #[test]
    fn empty_server_status_yields_empty_map() {
        let body = r#"{ "serverStatus": {} }"#;
        let map = parse_server_status(body).expect("present but empty");
        assert!(map.is_empty());
    }

    #[test]
    fn status_match_is_case_insensitive() {
        let body = r#"{ "serverStatus": {
            "http://a:1": "up", "http://b:2": "Down"
        } }"#;
        let map = parse_server_status(body).unwrap();
        assert_eq!(map.get("http://a:1"), Some(&true));
        assert_eq!(map.get("http://b:2"), Some(&false));
    }

    #[test]
    fn split_userinfo_extracts_basic_auth() {
        let (base, auth) =
            split_userinfo("http://admin:s3cret@rise-traefik:8080/").expect("parses");
        assert_eq!(base, "http://rise-traefik:8080");
        assert_eq!(auth, Some(("admin".to_string(), "s3cret".to_string())));
    }

    #[test]
    fn split_userinfo_no_auth() {
        let (base, auth) = split_userinfo("http://rise-traefik:8080").expect("parses");
        assert_eq!(base, "http://rise-traefik:8080");
        assert_eq!(auth, None);
    }

    #[test]
    fn split_userinfo_decodes_percent_encoded_creds() {
        // userinfo may be percent-encoded (e.g. a password containing '@').
        let (_base, auth) = split_userinfo("http://user:p%40ss@host:8080").expect("parses");
        assert_eq!(auth, Some(("user".to_string(), "p@ss".to_string())));
    }

    #[test]
    fn client_new_with_basic_auth_url() {
        let client =
            TraefikApiClient::new("http://admin:pw@rise-traefik:8080", "docker").expect("builds");
        assert_eq!(client.base_url, "http://rise-traefik:8080");
        assert_eq!(
            client.basic_auth,
            Some(("admin".to_string(), "pw".to_string()))
        );
    }
}
