//! Backend-agnostic runtime resolution shared by every deployment backend.
//!
//! These helpers turn a persisted [`Deployment`] row into the concrete inputs a
//! reconciler emits: the container/route layout, the resolved (decrypted)
//! environment variables, and the predicates/timeouts that gate when a
//! deployment should carry live infrastructure. They are pure over the core
//! models, the [`EncryptionProvider`] trait, and `rise-deployment-spec` — no
//! Kubernetes or Docker types — so both backends share one source of truth.

use std::collections::BTreeMap;

use anyhow::Context;
use rise_deployment_spec::project_config::{ImplicitAppContainer, ResolvedDeploy};
use rise_deployment_spec::request_spec::{ContainerSpec, RouteSpec};
use rise_deployment_spec::side_data::decode_side_data;

use crate::models::{Deployment, DeploymentEnvVar, DeploymentStatus};
use crate::providers::EncryptionProvider;
use crate::AccessRequirement;

/// Effective ingress auth requirement for a single route.
///
/// A route inherits the project's access-class requirement unless it carries a
/// per-route [`RouteSpec::access`] override (`.rise.toml` `[routes].access`).
/// This is the single home for the loosen/tighten rule so both the Kubernetes
/// and Docker reconcilers gate identically. `access` is a closed enum validated
/// at config load, so there is no fail-open "unknown value" path.
pub fn effective_access_requirement(
    route: &RouteSpec,
    project_requirement: &AccessRequirement,
) -> AccessRequirement {
    route
        .access
        .clone()
        .unwrap_or_else(|| project_requirement.clone())
}

/// Duration a deployment can be in Deploying state before timing out.
pub const DEPLOYING_TIMEOUT_MINUTES: i64 = 5;
/// Duration a deployment can be in pre-Pushed states before timing out.
pub const PRE_PUSHED_TIMEOUT_MINUTES: i64 = 10;

/// A deployment's environment variables, split into plain and (decrypted)
/// secret values. Backends adapt these neutral types to their own
/// representations (K8s `EnvVar`/`ByteString`, Docker `KEY=VALUE` strings).
#[derive(Debug, Default)]
pub struct ResolvedDeploymentEnvVars {
    /// `(name, value)` pairs injected directly into the container environment.
    pub plain_env_vars: Vec<(String, String)>,
    /// `name → decrypted-bytes` for values that must be carried as secrets.
    pub secret_env_vars: BTreeMap<String, Vec<u8>>,
}

/// Resolve the container + route list a reconciler should emit for a
/// deployment. A multi-container deployment parses its persisted side-data; a
/// single-container deployment (`containers IS NULL`) synthesises an implicit
/// `app` container from the row's columns plus a `/` route. The synthesised
/// `app` carries no image (`image: None`) — the reconciler fills it from the
/// deployment's resolved image, so the existing single-container image (tagged
/// with the deployment ID) is reused with no rebuild.
///
/// `containers`/`routes` come folded onto the [`Deployment`] row. A `None`
/// `containers` column is a legitimate single-container deployment and triggers
/// the synthesised `app`. A `Some` column that fails to deserialize is treated
/// as a hard error for *this* deployment (returned `Err`): silently collapsing
/// it to the single-container fallback would drop every per-container resource.
pub fn resolve_runtime_containers(
    deployment: &Deployment,
) -> anyhow::Result<(Vec<ContainerSpec>, Vec<RouteSpec>)> {
    if let Some(containers_value) = deployment.containers.as_ref() {
        let specs: Vec<ContainerSpec> = decode_side_data(containers_value).with_context(|| {
            format!(
                "deployment {} ({}) has a non-NULL `containers` column that could not be \
                 decoded into Vec<ContainerSpec>",
                deployment.id, deployment.deployment_id
            )
        })?;
        if specs.is_empty() {
            anyhow::bail!(
                "deployment {} ({}) has a non-NULL `containers` column with an empty items list; \
                 this is a degenerate state that should have been rejected at write time",
                deployment.id,
                deployment.deployment_id
            );
        }
        let routes: Vec<RouteSpec> = match deployment.routes.as_ref() {
            Some(routes_value) => decode_side_data(routes_value).with_context(|| {
                format!(
                    "deployment {} ({}) has a non-NULL `routes` column that could not be \
                     decoded into Vec<RouteSpec>",
                    deployment.id, deployment.deployment_id
                )
            })?,
            None => Vec::new(),
        };
        return Ok((specs, routes));
    }

    // Single-container deployment: synthesise the implicit `app` container.
    // A single-container deployment always has an http port (NOT NULL, default
    // 8080), so it is always routable at `/`.
    let resolved = ResolvedDeploy::implicit_app(ImplicitAppContainer {
        port: deployment.http_port as u16,
        replicas: Some(deployment.replicas as u32),
        cpu: Some(deployment.cpu.clone()),
        memory: Some(deployment.memory.clone()),
        ..Default::default()
    });
    let specs = resolved
        .containers
        .into_iter()
        .map(|container| ContainerSpec {
            name: container.name,
            image: container.image,
            port: container.port,
            replicas: container.replicas,
            cpu: container.cpu,
            memory: container.memory,
            env_overrides: Vec::new(),
            health_check: None,
        })
        .collect();
    let routes = resolved
        .routes
        .into_iter()
        .map(|route| RouteSpec {
            path: route.path,
            container: route.container,
            access: route.access,
        })
        .collect();
    Ok((specs, routes))
}

/// Returns true if this deployment should carry live infrastructure (the
/// backend's workload resources, workload-identity tokens, etc.).
pub fn should_have_infrastructure(deployment: &Deployment) -> bool {
    matches!(
        deployment.status,
        DeploymentStatus::Pushed
            | DeploymentStatus::Deploying
            | DeploymentStatus::Healthy
            | DeploymentStatus::Unhealthy
            | DeploymentStatus::Terminating
    )
}

/// Load and decrypt a deployment's environment variables into the neutral
/// [`ResolvedDeploymentEnvVars`] split. Secret values are decrypted via the
/// supplied [`EncryptionProvider`]; a secret with no provider configured is a
/// hard error.
pub async fn resolve_deployment_env_vars(
    env_vars: Vec<DeploymentEnvVar>,
    encryption_provider: Option<&dyn EncryptionProvider>,
) -> anyhow::Result<ResolvedDeploymentEnvVars> {
    let mut resolved = ResolvedDeploymentEnvVars::default();

    for var in env_vars {
        let key = var.key.trim().to_string();
        if key != var.key {
            tracing::warn!(
                "Environment variable key {:?} has surrounding whitespace; trimming to {:?}",
                var.key,
                key
            );
        }

        let value = if var.is_secret {
            match encryption_provider {
                Some(provider) => provider
                    .decrypt(&var.value)
                    .await
                    .with_context(|| format!("Failed to decrypt secret variable '{}'", key))?,
                None => {
                    tracing::error!(
                        "Encountered secret variable '{}' but no encryption provider configured",
                        key
                    );
                    return Err(anyhow::anyhow!(
                        "Cannot decrypt secret variable '{}': no encryption provider",
                        key
                    ));
                }
            }
        } else {
            var.value
        };

        if var.is_secret {
            resolved.secret_env_vars.insert(key, value.into_bytes());
        } else {
            resolved.plain_env_vars.push((key, value));
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn effective_access_requirement_prefers_route_override() {
        let mut route = RouteSpec {
            path: "/".to_string(),
            container: "app".to_string(),
            access: None,
        };
        // No override → inherit the project's requirement.
        assert_eq!(
            effective_access_requirement(&route, &AccessRequirement::Member),
            AccessRequirement::Member
        );
        // Override can loosen …
        route.access = Some(AccessRequirement::None);
        assert_eq!(
            effective_access_requirement(&route, &AccessRequirement::Member),
            AccessRequirement::None
        );
        // … or tighten.
        route.access = Some(AccessRequirement::Member);
        assert_eq!(
            effective_access_requirement(&route, &AccessRequirement::None),
            AccessRequirement::Member
        );
    }

    /// Identity-passthrough provider: `decrypt` returns the ciphertext verbatim
    /// so tests can assert which values were routed through decryption.
    struct PassthroughProvider;

    #[async_trait]
    impl EncryptionProvider for PassthroughProvider {
        async fn encrypt(&self, plaintext: &str) -> anyhow::Result<String> {
            Ok(plaintext.to_string())
        }
        async fn decrypt(&self, ciphertext: &str) -> anyhow::Result<String> {
            Ok(ciphertext.to_string())
        }
    }

    fn env_var(key: &str, value: &str, is_secret: bool) -> DeploymentEnvVar {
        DeploymentEnvVar {
            id: Uuid::new_v4(),
            deployment_id: Uuid::new_v4(),
            key: key.to_string(),
            value: value.to_string(),
            is_secret,
            is_protected: false,
            source: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn splits_plain_and_secret_and_trims_keys() {
        let env_vars = vec![
            env_var(" API_KEY ", "ciphertext-a", true),
            env_var("PORT", "8080", false),
        ];

        let resolved = resolve_deployment_env_vars(env_vars, Some(&PassthroughProvider))
            .await
            .unwrap();

        assert_eq!(
            resolved.plain_env_vars,
            vec![("PORT".to_string(), "8080".to_string())]
        );
        assert_eq!(resolved.secret_env_vars.len(), 1);
        assert_eq!(
            resolved.secret_env_vars["API_KEY"],
            b"ciphertext-a".to_vec()
        );
    }

    #[tokio::test]
    async fn secret_without_provider_errors() {
        let err = resolve_deployment_env_vars(vec![env_var("S", "x", true)], None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no encryption provider"));
    }
}
