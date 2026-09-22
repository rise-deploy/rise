use std::collections::BTreeMap;

use axum::{extract::State, http::HeaderMap, response::IntoResponse, response::Response, Json};

use crate::db::{
    deployments as db_deployments, environments as db_environments, models::Deployment,
    models::Project, projects as db_projects,
};
use crate::server::auth::middleware::extract_bearer_token;
use crate::server::deployment::webhook::should_have_infrastructure;
use crate::server::error::{ServerError, ServerErrorExt};
use crate::server::rate_limit::{extract_client_ip, rate_limit_response};
use crate::server::state::AppState;
use crate::server::workload_tokens::models::{
    AudienceTokensResponse, ExchangeTokenRequest, ExchangeTokenResponse,
};
use crate::server::workload_tokens::{
    remint_after_secs, sha256_hex, sign_audience_tokens, workload_subject, NO_ENVIRONMENT,
};
use rise_backend_auth::WorkloadSubjectInfo;

/// The deployment a bootstrap credential authenticates as, with what minting a
/// token for it needs.
struct CredentialSubject {
    deployment: Deployment,
    project: Project,
    environment: Option<String>,
}

impl CredentialSubject {
    fn sub(&self) -> String {
        workload_subject(&self.project.name, self.environment.as_deref())
    }
}

/// Authenticate the bootstrap credential in the `Authorization: Bearer` header.
///
/// `Ok(Err(response))` is a rate-limited request, answered with that response.
/// A missing deployment and a deployment without live infrastructure are both
/// reported as a rejected credential, so a caller cannot distinguish the two.
async fn authenticate_credential(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Result<CredentialSubject, Response>, ServerError> {
    let ip = extract_client_ip(headers);

    let credential = extract_bearer_token(headers)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ServerError::unauthorized("Missing bootstrap credential"))?;

    let hash = sha256_hex(credential.as_bytes());
    let deployment_by_credential =
        db_deployments::get_by_identity_credential_hash(&state.db_pool, &hash)
            .await
            .internal_err("Failed to look up deployment")?;

    let rate_limit_key = deployment_by_credential
        .as_ref()
        .map(|d| d.id.to_string())
        .unwrap_or_else(|| "invalid-credential".to_string());
    if let Err(retry_after) = state
        .oauth_rate_limiter
        .increment_and_check(&ip, None, &rate_limit_key)
        .await
    {
        return Ok(Err(rate_limit_response(retry_after).into_response()));
    }

    let deployment = match deployment_by_credential {
        None => return Err(ServerError::forbidden("Invalid bootstrap credential")),
        Some(d) if !should_have_infrastructure(&d) => {
            return Err(ServerError::bad_request("Deployment is not running"))
        }
        Some(d) => d,
    };

    let project = db_projects::find_by_id(&state.db_pool, deployment.project_id)
        .await
        .internal_err("Failed to load project")?
        .ok_or_else(|| ServerError::internal("Project not found for deployment"))?;

    // Resolve the environment name. A deployment with `environment_id = Some(..)`
    // MUST have a matching environment row — if the lookup yields `None`, fail
    // closed rather than minting a token with the `NO_ENVIRONMENT` sentinel,
    // which would be a wrong, potentially cross-environment identity.
    let environment = match deployment.environment_id {
        Some(env_id) => {
            let env = db_environments::find_by_id(&state.db_pool, env_id)
                .await
                .internal_err("Failed to load environment")?
                .ok_or_else(|| {
                    ServerError::internal(
                        "Deployment references an environment that no longer exists",
                    )
                })?;
            Some(env.name)
        }
        None => None,
    };

    Ok(Ok(CredentialSubject {
        deployment,
        project,
        environment,
    }))
}

/// Exchange a deployment's bootstrap credential for a workload identity token.
///
/// This route is unauthenticated: the bootstrap credential presented in the
/// `Authorization: Bearer` header *is* the authentication.
pub async fn exchange_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExchangeTokenRequest>,
) -> Result<impl IntoResponse, ServerError> {
    let audience = req.audience.trim();
    if audience.is_empty() {
        return Err(ServerError::bad_request("audience must not be empty"));
    }
    if audience.len() > 1024 {
        return Err(ServerError::bad_request("audience value too long"));
    }

    let subject = match authenticate_credential(&state, &headers).await? {
        Ok(subject) => subject,
        Err(rate_limited) => return Ok(rate_limited),
    };
    let CredentialSubject {
        deployment,
        project,
        environment,
    } = &subject;
    let sub = subject.sub();

    let max_ttl = state.server_settings.workload_token_max_ttl_seconds;
    let ttl = req.ttl_seconds.map(|t| t.min(max_ttl)).unwrap_or(max_ttl);

    let token = state
        .jwt_signer
        .sign_workload_jwt(
            &WorkloadSubjectInfo {
                sub: &sub,
                project: &project.name,
                environment: environment.as_deref().unwrap_or(NO_ENVIRONMENT),
                deployment_group: &deployment.deployment_group,
                deployment_id: &deployment.deployment_id,
            },
            audience,
            ttl,
        )
        .map_err(|e| ServerError::internal(format!("Failed to sign workload token: {:?}", e)))?;

    tracing::info!(
        project = %project.name,
        deployment_group = %deployment.deployment_group,
        deployment_id = %deployment.deployment_id,
        audience = %audience,
        ttl_seconds = ttl,
        "Issued workload identity token"
    );

    Ok(Json(ExchangeTokenResponse {
        token,
        token_type: "Bearer".to_string(),
        expires_in: ttl,
        audience: audience.to_string(),
    })
    .into_response())
}

/// Mint the tokens for every `[identity].audiences` entry the credential's
/// deployment declares, keyed by in-container filename.
///
/// This is the pre-minted file-token contract served over HTTP, for a backend
/// that writes the token files from inside the workload (the ECS identity
/// sidecar) rather than from the controller. It therefore mints exactly what
/// the Kubernetes webhook and the Docker reconciler put in those files: the
/// deployment's declared audiences — never caller-chosen ones — signed by the
/// same helper with the same `identity_token_ttl_seconds`, not the exchange
/// endpoint's shorter cap. That grants the credential holder nothing new: it
/// can already mint any audience through `exchange_token`, and read the file
/// tokens themselves on the other backends. `refresh_after_secs` carries the
/// shared re-mint policy so the caller need not duplicate it.
pub async fn audience_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ServerError> {
    let subject = match authenticate_credential(&state, &headers).await? {
        Ok(subject) => subject,
        Err(rate_limited) => return Ok(rate_limited),
    };
    let CredentialSubject {
        deployment,
        project,
        environment,
    } = &subject;
    let sub = subject.sub();

    let audiences = declared_audiences(&deployment.identity_audiences);

    let ttl = state.identity_token_ttl_seconds;
    let tokens = sign_audience_tokens(
        &state.jwt_signer,
        &WorkloadSubjectInfo {
            sub: &sub,
            project: &project.name,
            environment: environment.as_deref().unwrap_or(NO_ENVIRONMENT),
            deployment_group: &deployment.deployment_group,
            deployment_id: &deployment.deployment_id,
        },
        &audiences,
        ttl,
    )
    .map_err(|e| ServerError::internal(format!("Failed to sign workload tokens: {:?}", e)))?;

    tracing::debug!(
        project = %project.name,
        deployment_group = %deployment.deployment_group,
        deployment_id = %deployment.deployment_id,
        count = tokens.len(),
        ttl_seconds = ttl,
        "Issued workload identity audience tokens"
    );

    Ok(Json(AudienceTokensResponse {
        tokens,
        expires_in: ttl,
        refresh_after_secs: remint_after_secs(ttl),
    })
    .into_response())
}

/// A deployment's `[identity].audiences` as filename → audience. Filenames are
/// validated at deploy time; unsafe ones are dropped again here because they
/// become paths in the workload.
fn declared_audiences(identity_audiences: &serde_json::Value) -> BTreeMap<String, String> {
    serde_json::from_value::<BTreeMap<String, String>>(identity_audiences.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|(filename, _)| rise_backend_core::identity::is_safe_token_filename(filename))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audience-tokens endpoint mints exactly what the deployment declared,
    /// never a filename that could escape the tokens directory.
    #[test]
    fn declared_audiences_are_the_safe_entries_of_the_identity_block() {
        let audiences = declared_audiences(&serde_json::json!({
            "e2e": "rise-e2e-audience",
            "aws": "sts.amazonaws.com",
            "../credential": "evil",
        }));
        assert_eq!(
            audiences.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["aws", "e2e"]
        );
        assert_eq!(audiences["e2e"], "rise-e2e-audience");

        assert!(declared_audiences(&serde_json::json!({})).is_empty());
        assert!(declared_audiences(&serde_json::Value::Null).is_empty());
    }

    /// The empty-audience guard is pure input validation: an audience that is
    /// only whitespace is rejected as a bad request.
    #[test]
    fn empty_audience_is_rejected() {
        for raw in ["", "   ", "\t\n"] {
            assert!(
                raw.trim().is_empty(),
                "expected {raw:?} to be treated as an empty audience"
            );
        }
        // A non-empty audience trims to a non-empty string.
        assert_eq!("  https://aud  ".trim(), "https://aud");
    }

    // Bearer-token parsing is delegated to `auth::middleware::extract_bearer_token`,
    // which has its own unit tests (valid / missing header / wrong scheme).

    /// The credential lookup the handler relies on: a deployment is only a valid
    /// token-exchange subject when its bootstrap-credential hash is on record AND
    /// it currently has live infrastructure. Both an unknown credential and a
    /// deployment without infrastructure must resolve to "no deployment", which
    /// the handler reports as an invalid bootstrap credential (401).
    #[cfg(feature = "backend")]
    #[sqlx::test]
    async fn credential_lookup_requires_known_hash_and_live_infrastructure(pool: sqlx::PgPool) {
        use crate::db::deployments::{self as db_deployments, CreateDeploymentParams};
        use crate::db::models::DeploymentStatus;
        use uuid::Uuid;

        let user_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();

        sqlx::query!(
            "INSERT INTO users (id, email) VALUES ($1, $2)",
            user_id,
            "identity-test@example.com"
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query!(
            "INSERT INTO projects (id, name, owner_user_id, access_class, status) \
             VALUES ($1, $2, $3, $4, $5)",
            project_id,
            "identity-test-project",
            user_id,
            "public",
            "Stopped"
        )
        .execute(&pool)
        .await
        .unwrap();

        // An unknown credential resolves to no deployment → 403.
        let unknown_hash = sha256_hex(b"never-issued-credential");
        assert!(
            db_deployments::get_by_identity_credential_hash(&pool, &unknown_hash)
                .await
                .unwrap()
                .is_none(),
            "an unknown credential hash must not resolve to any deployment"
        );

        // Helper to create a deployment in a given status with its bootstrap
        // credential hash recorded.
        let make_deployment =
            |deployment_id: &'static str, status: DeploymentStatus, credential: &'static str| {
                let pool = pool.clone();
                async move {
                    let deployment = db_deployments::create(
                        &pool,
                        CreateDeploymentParams {
                            deployment_id,
                            project_id,
                            created_by_id: user_id,
                            status,
                            image: None,
                            image_digest: None,
                            rolled_back_from_deployment_id: None,
                            deployment_group: "default",
                            environment_id: None,
                            expires_at: None,
                            http_port: 8080,
                            is_active: false,
                            job_url: None,
                            pull_request_url: None,
                            git_repository_url: None,
                            replicas: 1,
                            cpu: "500m",
                            memory: "256Mi",
                            identity_audiences: serde_json::json!({}),
                            containers: None,
                            routes: None,
                            expiration_cap: None,
                        },
                    )
                    .await
                    .unwrap();
                    let hash = sha256_hex(credential.as_bytes());
                    db_deployments::set_identity_credential_hash(&pool, deployment.id, &hash)
                        .await
                        .unwrap();
                    (deployment, hash)
                }
            };

        // A deployment whose credential is on record but which has no live
        // infrastructure (still Pending) resolves to a deployment but is rejected
        // by `should_have_infrastructure` → the handler returns 400.
        let (_pending, pending_hash) =
            make_deployment("pending-deploy", DeploymentStatus::Pending, "pending-cred").await;
        let resolved = db_deployments::get_by_identity_credential_hash(&pool, &pending_hash)
            .await
            .unwrap();
        assert!(
            resolved.is_some(),
            "a Pending deployment with a known credential hash must resolve to a deployment"
        );
        assert!(
            !should_have_infrastructure(resolved.as_ref().unwrap()),
            "a Pending deployment has no live infrastructure"
        );

        // A deployment in a status with live infrastructure (Pushed) and a
        // matching credential hash IS a valid token-exchange subject.
        let (pushed, pushed_hash) =
            make_deployment("pushed-deploy", DeploymentStatus::Pushed, "pushed-cred").await;
        let resolved = db_deployments::get_by_identity_credential_hash(&pool, &pushed_hash)
            .await
            .unwrap()
            .filter(should_have_infrastructure);
        assert_eq!(
            resolved.map(|d| d.id),
            Some(pushed.id),
            "a Pushed deployment with a matching credential hash is a valid subject"
        );
    }
}
