//! Enforcement of scoped CLI sessions on the typed API (ADR-0006 §7).
//!
//! A session carrying `authorization_details` is narrowed to the grants it
//! names. [`scope_guard`] sits behind the auth middleware on every typed route
//! and classifies each route by method and matched path:
//!
//! - **ambient** routes are open to any session;
//! - **project**, **environment**, **deployment** and **env var** routes are
//!   decided here, from the path, the query, and (for a deployment) its
//!   environment;
//! - **handler** routes need what only the handler knows — a request body, or
//!   a list to filter — so the handler consults [`SessionScope`] itself, and
//!   the guard refuses a successful response from a handler that never did;
//! - everything else, unclassified routes included, needs an unrestricted
//!   session.
//!
//! The checks run in addition to each handler's own ownership checks, and the
//! admin bypass skips only those: a scoped admin session is as narrow as any.

use crate::db::{deployments, environments, projects};
use crate::server::error::ServerError;
use crate::server::state::AppState;
use axum::{
    extract::{FromRequestParts, MatchedPath, RawPathParams, Request, State},
    http::{request::Parts, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rise_backend_auth::session_scope::{
    AccessRequest, Kind, Operation, SessionCeiling, Subresource, Target, Verb,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The ceiling of the request's session, present only on a scoped session.
#[derive(Clone, Debug)]
pub struct SessionScope {
    ceiling: Arc<SessionCeiling>,
    organization: Arc<str>,
    /// Set by every decision, so the guard can tell that a handler route
    /// actually consulted the ceiling.
    consulted: Arc<AtomicBool>,
}

impl SessionScope {
    pub fn new(ceiling: SessionCeiling, organization: &str) -> Self {
        Self {
            ceiling: Arc::new(ceiling),
            organization: organization.into(),
            consulted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the ceiling allows `op` in `project` (see [`Target`] for what
    /// `environment` means per kind).
    pub fn allows(&self, op: Operation, project: &str, environment: Option<&str>) -> bool {
        self.consulted.store(true, Ordering::Relaxed);
        self.ceiling.allows(
            op,
            &Target {
                organization: &self.organization,
                project,
                environment,
            },
        )
    }

    /// [`Self::allows`], as a 403 naming what the session lacks.
    pub fn require(
        &self,
        op: Operation,
        project: &str,
        environment: Option<&str>,
    ) -> Result<(), ServerError> {
        if self.allows(op, project, environment) {
            Ok(())
        } else {
            Err(denied(op, project, environment))
        }
    }

    /// [`Self::may_reach`], as a 403 when the session holds nothing of the
    /// kind in `project`.
    pub fn require_reach(&self, op: Operation, project: &str) -> Result<(), ServerError> {
        if self.may_reach(op, project) {
            Ok(())
        } else {
            Err(denied(op, project, None))
        }
    }

    /// The grants the session holds, for display.
    pub fn access(&self) -> AccessRequest {
        self.ceiling.to_access()
    }

    /// Whether the session holds `op` on anything in `project`: a collection
    /// there may be listed and filtered per item with [`Self::allows`].
    pub fn may_reach(&self, op: Operation, project: &str) -> bool {
        self.consulted.store(true, Ordering::Relaxed);
        self.ceiling.may_reach(op, &self.organization, project)
    }
}

/// The request's [`SessionScope`], or `None` for an unrestricted credential.
pub struct MaybeSessionScope(pub Option<SessionScope>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeSessionScope {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<SessionScope>().cloned()))
    }
}

fn denied(op: Operation, project: &str, environment: Option<&str>) -> ServerError {
    let target = match environment {
        Some(environment) => format!("{project}/{environment}"),
        None => project.to_string(),
    };
    ServerError::forbidden(format!(
        "This login's access does not cover {} on '{target}'. Log in again with \
         `rise login --scope` to request it.",
        describe(op)
    ))
}

fn describe(op: Operation) -> String {
    let verb = format!("{:?}", op.verb).to_lowercase();
    let kind = format!("{:?}", op.kind);
    match op.subresource {
        Some(sub) => format!("{verb} {kind}/{}", format!("{sub:?}").to_lowercase()),
        None => format!("{verb} {kind}"),
    }
}

/// How the guard treats one route for a scoped session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// Open to any session.
    Ambient,
    /// Needs an unrestricted session.
    FullAccess,
    /// `op` on the project named in the path, outside any environment.
    OnProject(Operation),
    /// `op` on the environment named `{name}` in the path.
    OnEnvironment(Operation),
    /// `op` on a resource bound to the environment of the `{deployment_id}`
    /// in the path.
    OnDeployment(Operation),
    /// `op` on env vars of the `?environment=` in the query, or project-wide
    /// ones without it.
    OnEnvVar(Operation),
    /// The handler decides, and must consult the [`SessionScope`].
    Handler,
}

/// The rule for `method` on `path` (relative to `/api/v1`). Unknown routes
/// fall to [`Rule::FullAccess`].
fn classify(method: &Method, path: &str) -> Rule {
    use Kind::*;
    use Rule::*;
    use Verb::*;
    let op = Operation::new;
    let sub = Operation::sub;
    let m = method.as_str();
    match (m, path) {
        ("GET", "/users/me")
        | ("GET", "/logs/capabilities")
        | ("GET", "/platform/capabilities")
        | ("GET", "/extensions/types")
        | ("GET", "/quickstart-templates")
        | ("GET", "/projects/access-classes") => Ambient,

        ("GET", "/projects") => Handler,
        ("POST", "/deployments") => Handler,
        ("GET", "/projects/{project}/environments") => Handler,
        ("GET", "/projects/{project_name}/deployments") => Handler,

        ("GET", "/projects/{id_or_name}") => OnProject(op(Get, Project)),
        ("PUT", "/projects/{id_or_name}") => OnProject(op(Update, Project)),
        ("DELETE", "/projects/{id_or_name}") => OnProject(op(Delete, Project)),
        ("PATCH", "/projects/{id_or_name}/template-image") => OnProject(op(Update, Project)),

        ("POST", "/projects/{project}/environments") => OnProject(op(Create, Environment)),
        ("GET", "/projects/{project}/environments/{name}") => OnEnvironment(op(Get, Environment)),
        ("PATCH", "/projects/{project}/environments/{name}") => {
            OnEnvironment(op(Update, Environment))
        }
        ("DELETE", "/projects/{project}/environments/{name}") => {
            OnEnvironment(op(Delete, Environment))
        }

        // A group may span environments: project-wide only.
        ("GET", "/projects/{project_name}/deployment-groups") => OnProject(op(List, Deployment)),
        ("POST", "/projects/{project_name}/deployments/stop") => OnProject(op(Update, Deployment)),
        ("GET", "/projects/{project_name}/deployments/{deployment_id}")
        | ("GET", "/projects/{project_name}/deployments/{deployment_id}/containers")
        | ("GET", "/projects/{project_name}/deployments/{deployment_id}/events") => {
            OnDeployment(op(Get, Deployment))
        }
        ("PATCH", "/projects/{project_name}/deployments/{deployment_id}/status") => {
            OnDeployment(sub(Update, Deployment, Subresource::Status))
        }
        ("POST", "/projects/{project_name}/deployments/{deployment_id}/stop") => {
            OnDeployment(op(Update, Deployment))
        }
        ("GET", "/projects/{project_name}/deployments/{deployment_id}/logs")
        | ("GET", "/projects/{project_name}/deployments/{deployment_id}/logs/volume") => {
            OnDeployment(sub(Get, Deployment, Subresource::Logs))
        }
        ("GET", "/projects/{project_name}/deployments/{deployment_id}/registry-credentials") => {
            OnDeployment(sub(Create, Deployment, Subresource::RegistryCredentials))
        }
        ("GET", "/projects/{project_id_or_name}/deployments/{deployment_id}/env") => {
            OnDeployment(op(List, EnvironmentVariable))
        }
        ("GET", "/projects/{project_id_or_name}/deployments/{deployment_id}/env/{key}/value") => {
            OnDeployment(sub(Get, EnvironmentVariable, Subresource::Value))
        }

        ("GET", "/projects/{project_id_or_name}/env")
        | ("GET", "/projects/{project_id_or_name}/env/preview") => {
            OnEnvVar(op(List, EnvironmentVariable))
        }
        ("PUT", "/projects/{project_id_or_name}/env/{key}") => {
            OnEnvVar(op(Update, EnvironmentVariable))
        }
        ("DELETE", "/projects/{project_id_or_name}/env/{key}") => {
            OnEnvVar(op(Delete, EnvironmentVariable))
        }
        ("GET", "/projects/{project_id_or_name}/env/{key}/value") => {
            OnEnvVar(sub(Get, EnvironmentVariable, Subresource::Value))
        }
        // Moves between environments: project-wide only.
        ("POST", "/projects/{project_id_or_name}/env/{key}/move") => {
            OnProject(op(Update, EnvironmentVariable))
        }

        ("GET", "/projects/{project_id_or_name}/domains") => OnProject(op(List, CustomDomain)),
        ("POST", "/projects/{project_id_or_name}/domains") => OnProject(op(Create, CustomDomain)),
        ("GET", "/projects/{project_id_or_name}/domains/{domain}") => {
            OnProject(op(Get, CustomDomain))
        }
        ("PATCH", "/projects/{project_id_or_name}/domains/{domain}")
        | ("PUT", "/projects/{project_id_or_name}/domains/{domain}/primary")
        | ("DELETE", "/projects/{project_id_or_name}/domains/{domain}/primary") => {
            OnProject(op(Update, CustomDomain))
        }
        ("DELETE", "/projects/{project_id_or_name}/domains/{domain}") => {
            OnProject(op(Delete, CustomDomain))
        }

        ("GET", "/projects/{project_name}/service-accounts") => OnProject(op(List, ServiceAccount)),
        ("POST", "/projects/{project_name}/service-accounts") => {
            OnProject(op(Create, ServiceAccount))
        }
        ("GET", "/projects/{project_name}/service-accounts/{sa_id}") => {
            OnProject(op(Get, ServiceAccount))
        }
        ("PUT", "/projects/{project_name}/service-accounts/{sa_id}") => {
            OnProject(op(Update, ServiceAccount))
        }
        ("DELETE", "/projects/{project_name}/service-accounts/{sa_id}") => {
            OnProject(op(Delete, ServiceAccount))
        }

        ("GET", "/projects/{project}/extensions") => OnProject(op(List, Extension)),
        ("GET", "/projects/{project}/extensions/{extension}") => OnProject(op(Get, Extension)),
        ("POST", "/projects/{project}/extensions/{extension}") => OnProject(op(Create, Extension)),
        ("PUT", "/projects/{project}/extensions/{extension}")
        | ("PATCH", "/projects/{project}/extensions/{extension}") => {
            OnProject(op(Update, Extension))
        }
        ("DELETE", "/projects/{project}/extensions/{extension}") => {
            OnProject(op(Delete, Extension))
        }

        // Project creation, teams, `/encrypt`, user lookup, device approval,
        // the deprecated unscoped status route, and anything not listed.
        _ => FullAccess,
    }
}

/// Refuse what a scoped session's ceiling doesn't cover. A no-op for every
/// other credential.
pub async fn scope_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(scope) = req.extensions().get::<SessionScope>().cloned() else {
        return next.run(req).await;
    };
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().trim_start_matches("/api/v1").to_string())
        .unwrap_or_default();
    let rule = classify(req.method(), &path);
    match decide(&state, &scope, rule, req).await {
        Ok(Decision::Proceed(req)) => next.run(req).await,
        Ok(Decision::HandlerDecides(req)) => {
            let response = next.run(req).await;
            if response.status().is_success() && !scope.consulted.load(Ordering::Relaxed) {
                tracing::error!(
                    route = %path,
                    "Handler route answered a scoped session without consulting its scope"
                );
                return ServerError::internal("This request's access could not be checked")
                    .into_response();
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

enum Decision {
    Proceed(Request),
    HandlerDecides(Request),
}

async fn decide(
    state: &AppState,
    scope: &SessionScope,
    rule: Rule,
    req: Request,
) -> Result<Decision, ServerError> {
    let (mut parts, body) = req.into_parts();
    let params = RawPathParams::from_request_parts(&mut parts, state)
        .await
        .ok();
    let param = |name: &str| {
        params.as_ref().and_then(|params| {
            params
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    };
    let project_param = [
        "project",
        "project_name",
        "project_id_or_name",
        "id_or_name",
    ]
    .into_iter()
    .find_map(&param);

    match rule {
        Rule::Ambient => {}
        Rule::Handler => return Ok(Decision::HandlerDecides(Request::from_parts(parts, body))),
        Rule::FullAccess => {
            return Err(ServerError::forbidden(
                "This login is restricted to specific projects and environments and can't \
                 use this endpoint. Log in without `--scope` for full access.",
            ))
        }
        Rule::OnProject(op) => {
            let project = project_name(state, project_param).await?;
            scope.require(op, &project, None)?;
        }
        Rule::OnEnvironment(op) => {
            let project = project_name(state, project_param).await?;
            let environment = param("name");
            scope.require(op, &project, environment.as_deref())?;
        }
        Rule::OnEnvVar(op) => {
            let project = project_name(state, project_param).await?;
            let environment = query_param(&parts.uri, "environment");
            scope.require(op, &project, environment.as_deref())?;
        }
        Rule::OnDeployment(op) => {
            let project = project_name(state, project_param).await?;
            let environment = match param("deployment_id") {
                Some(deployment_id) => {
                    deployment_environment(state, &project, &deployment_id).await?
                }
                None => None,
            };
            scope.require(op, &project, environment.as_deref())?;
        }
    }
    Ok(Decision::Proceed(Request::from_parts(parts, body)))
}

/// The project's name from a path segment that may also be its UUID. A UUID
/// naming no project passes through unchanged: the handler answers 404.
async fn project_name(state: &AppState, param: Option<String>) -> Result<String, ServerError> {
    let param = param.ok_or_else(|| ServerError::internal("Route has no project parameter"))?;
    let Ok(id) = param.parse::<uuid::Uuid>() else {
        return Ok(param);
    };
    let project = projects::find_by_id(&state.db_pool, id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to resolve project {id} for a scoped session: {e:?}");
            ServerError::internal("Failed to look up project")
        })?;
    Ok(project.map(|p| p.name).unwrap_or(param))
}

/// The environment a deployment is bound to, by name. A deployment that
/// doesn't exist, or has no environment, has none: only a project-wide grant
/// covers it.
async fn deployment_environment(
    state: &AppState,
    project: &str,
    deployment_id: &str,
) -> Result<Option<String>, ServerError> {
    let lookup_failed = |e: anyhow::Error| {
        tracing::error!("Failed to resolve deployment environment for a scoped session: {e:?}");
        ServerError::internal("Failed to look up deployment")
    };
    let Some(project) = projects::find_by_name(&state.db_pool, project)
        .await
        .map_err(lookup_failed)?
    else {
        return Ok(None);
    };
    let Some(environment_id) =
        deployments::find_by_project_and_deployment_id(&state.db_pool, project.id, deployment_id)
            .await
            .map_err(lookup_failed)?
            .and_then(|deployment| deployment.environment_id)
    else {
        return Ok(None);
    };
    Ok(environments::find_by_id(&state.db_pool, environment_id)
        .await
        .map_err(lookup_failed)?
        .map(|environment| environment.name))
}

fn query_param(uri: &axum::http::Uri, name: &str) -> Option<String> {
    axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(uri)
        .ok()
        .and_then(|axum::extract::Query(mut params)| params.remove(name))
}

/// Compile what `user` grants into a session's `authorization_details`
/// (ADR-0006 §6): `None` for full access. Every target must exist and be one
/// `user` can reach, so a login never yields a session covering nothing.
pub async fn compile_grant(
    state: &AppState,
    user: &crate::db::User,
    request: &AccessRequest,
) -> Result<Option<Vec<serde_json::Value>>, ServerError> {
    request
        .validate()
        .map_err(|e| ServerError::bad_request(format!("Invalid access request: {e}")))?;
    if let AccessRequest::Restricted { grants } = request {
        let is_admin = state.is_admin(user).await;
        for grant in grants {
            let unreachable = || {
                ServerError::bad_request(format!(
                    "Project '{}' does not exist or you do not have access to it",
                    grant.project
                ))
            };
            let project = projects::find_by_name(&state.db_pool, &grant.project)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to look up a granted project: {e:?}");
                    ServerError::internal("Failed to look up project")
                })?
                .ok_or_else(unreachable)?;
            let can_access = is_admin
                || projects::user_can_access(&state.db_pool, project.id, user.id)
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to check access to a granted project: {e:?}");
                        ServerError::internal("Failed to check project access")
                    })?;
            if !can_access {
                return Err(unreachable());
            }
            if let Some(environment) = &grant.environment {
                environments::find_by_name(&state.db_pool, project.id, environment)
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to look up a granted environment: {e:?}");
                        ServerError::internal("Failed to look up environment")
                    })?
                    .ok_or_else(|| {
                        ServerError::bad_request(format!(
                            "Environment '{environment}' does not exist in project '{}'",
                            grant.project
                        ))
                    })?;
            }
        }
    }
    Ok(rise_backend_auth::session_scope::compile(
        &state.default_organization_name,
        request,
    ))
}

/// Refuse a scoped session on the generic resource API (ADR-0006 §7): its
/// engine doesn't evaluate session ceilings yet.
pub async fn refuse_on_resource_api(req: Request, next: Next) -> Response {
    if req.extensions().get::<SessionScope>().is_some() {
        return (
            StatusCode::FORBIDDEN,
            "Logins restricted with --scope can't use the resource API yet",
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_routes_need_full_access() {
        assert_eq!(classify(&Method::POST, "/projects"), Rule::FullAccess);
        assert_eq!(classify(&Method::GET, "/teams"), Rule::FullAccess);
        assert_eq!(classify(&Method::POST, "/encrypt"), Rule::FullAccess);
        assert_eq!(
            classify(&Method::PATCH, "/deployments/{deployment_id}/status"),
            Rule::FullAccess
        );
        assert_eq!(classify(&Method::GET, "/something/new"), Rule::FullAccess);
        // The right path with a method it doesn't serve is just as unknown.
        assert_eq!(
            classify(&Method::DELETE, "/projects/{project}/environments"),
            Rule::FullAccess
        );
    }

    #[test]
    fn deployment_routes_resolve_through_the_deployment() {
        assert_eq!(
            classify(
                &Method::GET,
                "/projects/{project_name}/deployments/{deployment_id}/registry-credentials"
            ),
            Rule::OnDeployment(Operation::sub(
                Verb::Create,
                Kind::Deployment,
                Subresource::RegistryCredentials
            ))
        );
        assert_eq!(classify(&Method::POST, "/deployments"), Rule::Handler);
    }

    #[test]
    fn query_param_decodes() {
        let uri: axum::http::Uri = "/x?a=1&environment=stag%69ng".parse().unwrap();
        assert_eq!(query_param(&uri, "environment").as_deref(), Some("staging"));
        let bare: axum::http::Uri = "/x".parse().unwrap();
        assert_eq!(query_param(&bare, "environment"), None);
    }
}
