//! Rise system environment variables injected into every deployed container.

use crate::backend::DeploymentUrls;
use crate::group::normalize_deployment_group;

/// Generate the Rise system environment variables for a deployment.
///
/// Returns `(key, value)` pairs for:
/// - `RISE_ISSUER` — Rise server URL (base URL for all Rise endpoints and JWT issuer)
/// - `RISE_APP_URL` — Canonical URL where the app is accessible (omitted when
///   the deployment's group has no public host)
/// - `RISE_APP_URLS` — JSON array of all URLs where the app can be accessed
/// - `RISE_DEPLOYMENT_GROUP` — The deployment group name (e.g. "default", "mr/123")
/// - `RISE_DEPLOYMENT_GROUP_NORMALIZED` — The group name normalized for URLs (e.g. "mr--123")
/// - `RISE_ENVIRONMENT` — The environment name (e.g. "production", "staging"), if set
pub fn rise_system_env_vars(
    public_url: &str,
    deployment_group: &str,
    deployment_urls: &DeploymentUrls,
    environment_name: Option<&str>,
) -> Vec<(String, String)> {
    let app_urls_json =
        serde_json::to_string(&deployment_urls.all_urls).unwrap_or_else(|_| "[]".to_string());

    let mut vars = vec![("RISE_ISSUER".to_string(), public_url.to_string())];
    // An unrouted group has no URL to advertise.
    if let Some(primary_url) = &deployment_urls.primary_url {
        vars.push(("RISE_APP_URL".to_string(), primary_url.clone()));
    }
    vars.extend([
        ("RISE_APP_URLS".to_string(), app_urls_json),
        (
            "RISE_DEPLOYMENT_GROUP".to_string(),
            deployment_group.to_string(),
        ),
        (
            "RISE_DEPLOYMENT_GROUP_NORMALIZED".to_string(),
            normalize_deployment_group(deployment_group),
        ),
    ]);

    if let Some(env_name) = environment_name {
        vars.push(("RISE_ENVIRONMENT".to_string(), env_name.to_string()));
    }

    vars
}
