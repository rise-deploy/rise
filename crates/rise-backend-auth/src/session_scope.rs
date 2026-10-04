//! Scoped CLI sessions (ADR-0006): the permission vocabulary a login requests,
//! its compilation into a session's `authorization_details` ceiling, and the
//! typed-API evaluation of that ceiling.
//!
//! The ceiling is ADR-0001 §7's wire format — `rise.dev/rbac` entries, each one
//! Scope plus permission statements — with ADR-0006 §4's optional static
//! `labelSelector` on `rise.dev/environment`. This module evaluates it for the
//! typed API only: it knows the closed set of typed kinds and the two Scope
//! shapes a session may name, and rejects anything else as an invalid
//! credential. The generic resource API refuses scoped sessions until those
//! kinds migrate (ADR-0006 §8).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The `type` of every ceiling entry.
pub const RBAC_DETAIL_TYPE: &str = "rise.dev/rbac";
/// The governed label binding a resource to its environment (ADR-0006 §4).
pub const ENVIRONMENT_LABEL: &str = "rise.dev/environment";
/// The API group of every typed kind.
const API_GROUP: &str = "rise.dev";
/// The most grants one request may carry.
const MAX_GRANTS: usize = 32;

/// Why an access request or a session ceiling is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ScopeError(String);

impl ScopeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// A named permission a grant carries (ADR-0006 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    View,
    Logs,
    Deploy,
    EnvVars,
    Secrets,
    Configure,
    ServiceAccounts,
    ProjectAdmin,
}

impl Permission {
    pub const ALL: [Permission; 8] = [
        Permission::View,
        Permission::Logs,
        Permission::Deploy,
        Permission::EnvVars,
        Permission::Secrets,
        Permission::Configure,
        Permission::ServiceAccounts,
        Permission::ProjectAdmin,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Permission::View => "view",
            Permission::Logs => "logs",
            Permission::Deploy => "deploy",
            Permission::EnvVars => "env-vars",
            Permission::Secrets => "secrets",
            Permission::Configure => "configure",
            Permission::ServiceAccounts => "service-accounts",
            Permission::ProjectAdmin => "project-admin",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// Whether an environment grant may carry this permission. The others
    /// reach beyond one environment (ADR-0006 §5).
    pub fn environment_allowed(self) -> bool {
        matches!(
            self,
            Permission::View
                | Permission::Logs
                | Permission::Deploy
                | Permission::EnvVars
                | Permission::Secrets
        )
    }

    /// The operations this permission allows, as `(verbs, kinds, subresource)`
    /// statements. On an environment grant only the environment-bound kinds
    /// remain: the rest of the target is reached through the grant's other
    /// entries (see [`compile`]).
    fn statements(self, environment: bool) -> Vec<Statement> {
        use Kind::*;
        use Verb::*;
        let all_kinds = [
            Project,
            Environment,
            Deployment,
            EnvironmentVariable,
            CustomDomain,
            Extension,
            ServiceAccount,
            AppUser,
        ];
        let statements = match self {
            Permission::View => vec![Statement::new(&[Get, List], &all_kinds, None)],
            Permission::Logs => vec![Statement::new(
                &[Get],
                &[Deployment],
                Some(Subresource::Logs),
            )],
            Permission::Deploy => vec![
                Statement::new(&[Create, Update], &[Deployment], None),
                Statement::new(&[Update], &[Deployment], Some(Subresource::Status)),
                Statement::new(
                    &[Create],
                    &[Deployment],
                    Some(Subresource::RegistryCredentials),
                ),
            ],
            Permission::EnvVars => vec![Statement::new(
                &[Create, Update, Delete],
                &[EnvironmentVariable],
                None,
            )],
            Permission::Secrets => vec![Statement::new(
                &[Get],
                &[EnvironmentVariable],
                Some(Subresource::Value),
            )],
            Permission::Configure => vec![Statement::new(
                &[Create, Update, Delete],
                &[Environment, CustomDomain, Extension, AppUser],
                None,
            )],
            Permission::ServiceAccounts => vec![
                Statement::new(&[Create, Update, Delete], &[ServiceAccount], None),
                Statement::new(&[Create], &[ServiceAccount], Some(Subresource::Token)),
            ],
            Permission::ProjectAdmin => vec![Statement::new(&[Update, Delete], &[Project], None)],
        };
        if !environment {
            return statements;
        }
        statements
            .into_iter()
            .filter_map(|mut statement| {
                statement.kinds.retain(|kind| kind.environment_bound());
                (!statement.kinds.is_empty()).then_some(statement)
            })
            .collect()
    }
}

/// A named bundle of permissions (ADR-0006 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    Read,
    Deploy,
    Develop,
    Admin,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Read, Preset::Deploy, Preset::Develop, Preset::Admin];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Read => "read",
            Preset::Deploy => "deploy",
            Preset::Develop => "develop",
            Preset::Admin => "admin",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    pub fn permissions(self) -> BTreeSet<Permission> {
        use Permission::*;
        match self {
            Preset::Read => [View].into(),
            Preset::Deploy => [View, Logs, Deploy].into(),
            Preset::Develop => [View, Logs, Deploy, EnvVars].into(),
            Preset::Admin => Permission::ALL.into(),
        }
    }

    /// The preset whose permissions are exactly `permissions`, if any.
    pub fn matching(permissions: &BTreeSet<Permission>) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| &preset.permissions() == permissions)
    }
}

/// One grant: a target and what may be done there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<Preset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
}

impl Grant {
    /// The union of the grant's preset and individual permissions.
    pub fn permission_set(&self) -> BTreeSet<Permission> {
        let mut set: BTreeSet<Permission> = self.permissions.iter().copied().collect();
        if let Some(preset) = self.preset {
            set.extend(preset.permissions());
        }
        set
    }

    /// `my-app` or `my-app/staging`.
    pub fn target_label(&self) -> String {
        match &self.environment {
            Some(environment) => format!("{}/{}", self.project, environment),
            None => self.project.clone(),
        }
    }
}

/// What a login asks for, and what an approval grants (ADR-0006 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessRequest {
    /// The user's full live access: no ceiling.
    Full,
    /// Only the listed grants.
    Restricted { grants: Vec<Grant> },
}

impl AccessRequest {
    /// Check the request's shape. Says nothing about whether its projects and
    /// environments exist: that is the approver's to resolve.
    pub fn validate(&self) -> Result<(), ScopeError> {
        let AccessRequest::Restricted { grants } = self else {
            return Ok(());
        };
        if grants.is_empty() {
            return Err(ScopeError::new(
                "a restricted request needs at least one grant",
            ));
        }
        if grants.len() > MAX_GRANTS {
            return Err(ScopeError::new(format!(
                "a request may carry at most {MAX_GRANTS} grants"
            )));
        }
        for grant in grants {
            validate_name("project", &grant.project)?;
            if let Some(environment) = &grant.environment {
                validate_name("environment", environment)?;
            }
            let permissions = grant.permission_set();
            if permissions.is_empty() {
                return Err(ScopeError::new(format!(
                    "grant on '{}' names no preset or permission",
                    grant.target_label()
                )));
            }
            if grant.environment.is_some() {
                if let Some(denied) = permissions.iter().find(|p| !p.environment_allowed()) {
                    return Err(ScopeError::new(format!(
                        "'{}' applies to a whole project and can't be granted on environment '{}'",
                        denied.name(),
                        grant.target_label()
                    )));
                }
            }
        }
        Ok(())
    }

    /// The request with grants on one target merged and in a stable order,
    /// each carrying its matching preset instead of a permission list where
    /// one fits.
    pub fn normalized(&self) -> AccessRequest {
        let AccessRequest::Restricted { grants } = self else {
            return AccessRequest::Full;
        };
        let mut merged: BTreeMap<(String, Option<String>), BTreeSet<Permission>> = BTreeMap::new();
        for grant in grants {
            merged
                .entry((grant.project.clone(), grant.environment.clone()))
                .or_default()
                .extend(grant.permission_set());
        }
        AccessRequest::Restricted {
            grants: merged
                .into_iter()
                .map(|((project, environment), permissions)| {
                    let preset = Preset::matching(&permissions);
                    Grant {
                        project,
                        environment,
                        preset,
                        permissions: if preset.is_some() {
                            Vec::new()
                        } else {
                            permissions.into_iter().collect()
                        },
                    }
                })
                .collect(),
        }
    }
}

/// Project and environment names: what Rise's own name rules allow, checked
/// loosely here — the approver resolves them against real resources.
fn validate_name(what: &str, name: &str) -> Result<(), ScopeError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(ScopeError::new(format!("invalid {what} name '{name}'")))
    }
}

/// Compile a validated request into a session's `authorization_details`.
/// `None` is full access: the claim is omitted.
///
/// A project grant is one entry at the project's Scope. An environment grant
/// is three (ADR-0006 §4): `get`/`list` on the Project, so the CLI can resolve
/// it; `get`/`list` on the Environment itself; and the granted statements on
/// the environment-bound kinds, narrowed by the `rise.dev/environment` label.
pub fn compile(organization: &str, request: &AccessRequest) -> Option<Vec<serde_json::Value>> {
    let AccessRequest::Restricted { grants } = request.normalized() else {
        return None;
    };
    let mut entries = Vec::new();
    for grant in grants {
        let permissions = grant.permission_set();
        let project_scope = format!("{API_GROUP}/Project/{organization}/{}", grant.project);
        match &grant.environment {
            None => entries.push(CeilingEntry {
                scope: project_scope,
                label_selector: None,
                permissions: permissions
                    .iter()
                    .flat_map(|p| p.statements(false))
                    .collect(),
            }),
            Some(environment) => {
                entries.push(CeilingEntry {
                    scope: project_scope.clone(),
                    label_selector: None,
                    permissions: vec![Statement::new(
                        &[Verb::Get, Verb::List],
                        &[Kind::Project],
                        None,
                    )],
                });
                entries.push(CeilingEntry {
                    scope: format!(
                        "{API_GROUP}/Environment/{organization}/{}/{environment}",
                        grant.project
                    ),
                    label_selector: None,
                    permissions: vec![Statement::new(
                        &[Verb::Get, Verb::List],
                        &[Kind::Environment],
                        None,
                    )],
                });
                entries.push(CeilingEntry {
                    scope: project_scope,
                    label_selector: Some(LabelSelector {
                        key: ENVIRONMENT_LABEL.to_string(),
                        value: environment.clone(),
                    }),
                    permissions: permissions
                        .iter()
                        .flat_map(|p| p.statements(true))
                        .collect(),
                });
            }
        }
    }
    Some(
        entries
            .into_iter()
            .map(|entry| {
                let mut value = serde_json::to_value(entry).expect("a ceiling entry serializes");
                value["type"] = serde_json::Value::String(RBAC_DETAIL_TYPE.to_string());
                value
            })
            .collect(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Get,
    List,
    Create,
    Update,
    Delete,
}

/// The typed kinds a session ceiling may name (ADR-0006 §3). They are not in
/// the resource API's registry; each is registered there when it migrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Project,
    Environment,
    Deployment,
    EnvironmentVariable,
    CustomDomain,
    Extension,
    ServiceAccount,
    AppUser,
}

impl Kind {
    const ALL: [Kind; 8] = [
        Kind::Project,
        Kind::Environment,
        Kind::Deployment,
        Kind::EnvironmentVariable,
        Kind::CustomDomain,
        Kind::Extension,
        Kind::ServiceAccount,
        Kind::AppUser,
    ];

    fn name(self) -> &'static str {
        match self {
            Kind::Project => "Project",
            Kind::Environment => "Environment",
            Kind::Deployment => "Deployment",
            Kind::EnvironmentVariable => "EnvironmentVariable",
            Kind::CustomDomain => "CustomDomain",
            Kind::Extension => "Extension",
            Kind::ServiceAccount => "ServiceAccount",
            Kind::AppUser => "AppUser",
        }
    }

    /// Kinds whose instances may carry the `rise.dev/environment` label.
    pub fn environment_bound(self) -> bool {
        matches!(self, Kind::Deployment | Kind::EnvironmentVariable)
    }
}

impl Serialize for Kind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{API_GROUP}/{}", self.name()))
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let qualified = String::deserialize(deserializer)?;
        qualified
            .strip_prefix(&format!("{API_GROUP}/"))
            .and_then(|name| Kind::ALL.into_iter().find(|kind| kind.name() == name))
            .ok_or_else(|| serde::de::Error::custom(format!("unknown kind '{qualified}'")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subresource {
    Logs,
    Value,
    RegistryCredentials,
    Status,
    Token,
}

/// One operation on a typed resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    pub verb: Verb,
    pub kind: Kind,
    pub subresource: Option<Subresource>,
}

impl Operation {
    pub const fn new(verb: Verb, kind: Kind) -> Self {
        Self {
            verb,
            kind,
            subresource: None,
        }
    }

    pub const fn sub(verb: Verb, kind: Kind, subresource: Subresource) -> Self {
        Self {
            verb,
            kind,
            subresource: Some(subresource),
        }
    }
}

/// The resource an operation acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target<'a> {
    pub organization: &'a str,
    pub project: &'a str,
    /// For an `Environment` operation, that environment's name. For an
    /// environment-bound kind, the resource's `rise.dev/environment` label:
    /// `None` for a deployment without an environment or a project-wide env
    /// var. Ignored for every other kind.
    pub environment: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Statement {
    verbs: BTreeSet<Verb>,
    kinds: BTreeSet<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subresources: Option<BTreeSet<Subresource>>,
}

impl Statement {
    fn new(verbs: &[Verb], kinds: &[Kind], subresource: Option<Subresource>) -> Self {
        Self {
            verbs: verbs.iter().copied().collect(),
            kinds: kinds.iter().copied().collect(),
            subresources: subresource.map(|s| [s].into()),
        }
    }

    fn matches(&self, op: &Operation) -> bool {
        self.verbs.contains(&op.verb)
            && self.kinds.contains(&op.kind)
            && match (&self.subresources, op.subresource) {
                (None, None) => true,
                (Some(subresources), Some(subresource)) => subresources.contains(&subresource),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelSelector {
    key: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CeilingEntry {
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label_selector: Option<LabelSelector>,
    permissions: Vec<Statement>,
}

/// The Scope shapes a session may name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ScopePath {
    Project {
        organization: String,
        project: String,
    },
    Environment {
        organization: String,
        project: String,
        environment: String,
    },
}

impl ScopePath {
    fn parse(scope: &str) -> Result<Self, ScopeError> {
        let invalid = || ScopeError::new(format!("unsupported scope '{scope}'"));
        let parts: Vec<&str> = scope.split('/').collect();
        if parts.iter().any(|part| part.is_empty()) || parts.first() != Some(&API_GROUP) {
            return Err(invalid());
        }
        match parts[1..] {
            ["Project", organization, project] => Ok(ScopePath::Project {
                organization: organization.to_string(),
                project: project.to_string(),
            }),
            ["Environment", organization, project, environment] => Ok(ScopePath::Environment {
                organization: organization.to_string(),
                project: project.to_string(),
                environment: environment.to_string(),
            }),
            _ => Err(invalid()),
        }
    }

    fn organization_and_project(&self) -> (&str, &str) {
        match self {
            ScopePath::Project {
                organization,
                project,
            }
            | ScopePath::Environment {
                organization,
                project,
                ..
            } => (organization, project),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedEntry {
    scope: ScopePath,
    /// The `rise.dev/environment` value the entry is narrowed to.
    environment: Option<String>,
    statements: Vec<Statement>,
}

impl ParsedEntry {
    fn reaches(&self, kind: Kind, target: &Target<'_>) -> bool {
        let in_scope = match &self.scope {
            ScopePath::Project {
                organization,
                project,
            } => organization == target.organization && project == target.project,
            ScopePath::Environment {
                organization,
                project,
                environment,
            } => {
                organization == target.organization
                    && project == target.project
                    && kind == Kind::Environment
                    && target.environment == Some(environment.as_str())
            }
        };
        in_scope
            && match &self.environment {
                None => true,
                Some(value) => {
                    kind.environment_bound() && target.environment == Some(value.as_str())
                }
            }
    }
}

/// A session's parsed Allow ceiling, evaluated on the typed API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCeiling {
    entries: Vec<ParsedEntry>,
}

impl SessionCeiling {
    /// Parse a session's `authorization_details`. Anything this module doesn't
    /// know — another entry type, kind, verb, subresource, Scope shape, or
    /// selector key — is an error, which the caller treats as an invalid
    /// credential rather than ignoring.
    pub fn parse(details: &[serde_json::Value]) -> Result<Self, ScopeError> {
        if details.is_empty() {
            return Err(ScopeError::new("authorization_details must not be empty"));
        }
        let entries = details
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let invalid =
                    |why: String| ScopeError::new(format!("authorization_details[{index}]: {why}"));
                let mut value = value.clone();
                let object = value
                    .as_object_mut()
                    .ok_or_else(|| invalid("not an object".into()))?;
                match object.remove("type") {
                    Some(serde_json::Value::String(t)) if t == RBAC_DETAIL_TYPE => {}
                    other => return Err(invalid(format!("unsupported type {other:?}"))),
                }
                let entry: CeilingEntry =
                    serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
                if entry.permissions.is_empty()
                    || entry.permissions.iter().any(|s| {
                        s.verbs.is_empty()
                            || s.kinds.is_empty()
                            || s.subresources.as_ref().is_some_and(BTreeSet::is_empty)
                    })
                {
                    return Err(invalid("empty permission list or axis".into()));
                }
                let environment = match entry.label_selector {
                    None => None,
                    Some(selector) if selector.key == ENVIRONMENT_LABEL => Some(selector.value),
                    Some(selector) => {
                        return Err(invalid(format!(
                            "unsupported label selector key '{}'",
                            selector.key
                        )))
                    }
                };
                Ok(ParsedEntry {
                    scope: ScopePath::parse(&entry.scope).map_err(|e| invalid(e.to_string()))?,
                    environment,
                    statements: entry.permissions,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { entries })
    }

    /// Whether the ceiling allows `op` on `target`.
    pub fn allows(&self, op: Operation, target: &Target<'_>) -> bool {
        self.entries.iter().any(|entry| {
            entry.reaches(op.kind, target) && entry.statements.iter().any(|s| s.matches(&op))
        })
    }

    /// Whether some entry inside `project` allows `op` on at least one
    /// resource there: a collection request may proceed and filter per item.
    pub fn may_reach(&self, op: Operation, organization: &str, project: &str) -> bool {
        self.entries.iter().any(|entry| {
            entry.scope.organization_and_project() == (organization, project)
                && entry.statements.iter().any(|s| s.matches(&op))
        })
    }

    /// The grants this ceiling amounts to, in the request shape: the targets
    /// it names and, for each, the permissions it covers in full.
    pub fn to_access(&self) -> AccessRequest {
        let mut targets: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
        for entry in &self.entries {
            let (organization, project) = entry.scope.organization_and_project();
            let environment = match (&entry.scope, &entry.environment) {
                (ScopePath::Environment { environment, .. }, _) => Some(environment.clone()),
                (_, Some(environment)) => Some(environment.clone()),
                _ => None,
            };
            targets.insert((organization.to_string(), project.to_string(), environment));
        }
        let mut grants = Vec::new();
        for (organization, project, environment) in targets {
            let target = Target {
                organization: &organization,
                project: &project,
                environment: environment.as_deref(),
            };
            let covered = |permission: Permission| {
                let env_scoped = environment.is_some();
                permission
                    .statements(env_scoped)
                    .iter()
                    .flat_map(|statement| {
                        statement.verbs.iter().flat_map(move |verb| {
                            statement.kinds.iter().map(move |kind| Operation {
                                verb: *verb,
                                kind: *kind,
                                subresource: statement
                                    .subresources
                                    .as_ref()
                                    .and_then(|s| s.iter().next().copied()),
                            })
                        })
                    })
                    .all(|op| {
                        // A project grant must cover every environment, so it
                        // is checked without a label; an environment grant
                        // with its own.
                        let probe = Target {
                            environment: if env_scoped { target.environment } else { None },
                            ..target
                        };
                        self.allows(op, &probe)
                    })
            };
            let permissions: Vec<Permission> = Permission::ALL
                .into_iter()
                .filter(|p| (environment.is_none() || p.environment_allowed()) && covered(*p))
                .collect();
            if !permissions.is_empty() {
                grants.push(Grant {
                    project,
                    environment,
                    preset: None,
                    permissions,
                });
            }
        }
        // A ceiling covering nothing nameable still restricts: never `Full`.
        AccessRequest::Restricted { grants }.normalized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG: &str = "default";

    fn grant(project: &str, environment: Option<&str>, preset: Preset) -> Grant {
        Grant {
            project: project.into(),
            environment: environment.map(Into::into),
            preset: Some(preset),
            permissions: vec![],
        }
    }

    fn ceiling(grants: Vec<Grant>) -> SessionCeiling {
        let request = AccessRequest::Restricted { grants };
        request.validate().unwrap();
        SessionCeiling::parse(&compile(ORG, &request).unwrap()).unwrap()
    }

    fn at<'a>(project: &'a str, environment: Option<&'a str>) -> Target<'a> {
        Target {
            organization: ORG,
            project,
            environment,
        }
    }

    const CREATE_DEPLOYMENT: Operation = Operation::new(Verb::Create, Kind::Deployment);
    const GET_DEPLOYMENT: Operation = Operation::new(Verb::Get, Kind::Deployment);

    #[test]
    fn full_access_compiles_to_no_claim() {
        assert_eq!(compile(ORG, &AccessRequest::Full), None);
    }

    #[test]
    fn environment_grant_deploys_only_into_its_environment() {
        let c = ceiling(vec![grant("app", Some("staging"), Preset::Deploy)]);
        assert!(c.allows(CREATE_DEPLOYMENT, &at("app", Some("staging"))));
        assert!(c.allows(GET_DEPLOYMENT, &at("app", Some("staging"))));
        assert!(c.allows(
            Operation::sub(Verb::Get, Kind::Deployment, Subresource::Logs),
            &at("app", Some("staging"))
        ));
        // Another environment, no environment, another project.
        assert!(!c.allows(CREATE_DEPLOYMENT, &at("app", Some("production"))));
        assert!(!c.allows(GET_DEPLOYMENT, &at("app", Some("production"))));
        assert!(!c.allows(CREATE_DEPLOYMENT, &at("app", None)));
        assert!(!c.allows(GET_DEPLOYMENT, &at("other", Some("staging"))));
    }

    #[test]
    fn environment_grant_sees_its_project_and_environment_only() {
        let c = ceiling(vec![grant("app", Some("staging"), Preset::Develop)]);
        let get_project = Operation::new(Verb::Get, Kind::Project);
        let get_env = Operation::new(Verb::Get, Kind::Environment);
        assert!(c.allows(get_project, &at("app", None)));
        assert!(c.allows(get_env, &at("app", Some("staging"))));
        assert!(!c.allows(get_env, &at("app", Some("production"))));
        assert!(!c.allows(get_project, &at("other", None)));
        // Nothing project-wide or environment-changing.
        for op in [
            Operation::new(Verb::Update, Kind::Project),
            Operation::new(Verb::Update, Kind::Environment),
            Operation::new(Verb::Get, Kind::CustomDomain),
            Operation::new(Verb::Create, Kind::ServiceAccount),
        ] {
            assert!(!c.allows(op, &at("app", Some("staging"))), "{op:?}");
            assert!(!c.allows(op, &at("app", None)), "{op:?}");
        }
        // Env vars: the environment's own, never project-wide ones.
        let set_var = Operation::new(Verb::Update, Kind::EnvironmentVariable);
        assert!(c.allows(set_var, &at("app", Some("staging"))));
        assert!(!c.allows(set_var, &at("app", None)));
    }

    #[test]
    fn project_grant_covers_every_environment() {
        let c = ceiling(vec![grant("app", None, Preset::Deploy)]);
        for environment in [Some("staging"), Some("production"), None] {
            assert!(c.allows(CREATE_DEPLOYMENT, &at("app", environment)));
        }
        assert!(!c.allows(
            Operation::new(Verb::Update, Kind::Project),
            &at("app", None)
        ));
        assert!(!c.allows(CREATE_DEPLOYMENT, &at("other", None)));
    }

    #[test]
    fn project_wide_permissions_are_refused_on_environments() {
        let request = AccessRequest::Restricted {
            grants: vec![Grant {
                project: "app".into(),
                environment: Some("staging".into()),
                preset: None,
                permissions: vec![Permission::Configure],
            }],
        };
        assert!(request.validate().is_err());
        assert!(AccessRequest::Restricted {
            grants: vec![grant("app", Some("staging"), Preset::Admin)]
        }
        .validate()
        .is_err());
        assert!(AccessRequest::Restricted { grants: vec![] }
            .validate()
            .is_err());
    }

    #[test]
    fn unknown_ceiling_content_is_an_invalid_credential() {
        let valid = compile(
            ORG,
            &AccessRequest::Restricted {
                grants: vec![grant("app", Some("staging"), Preset::Read)],
            },
        )
        .unwrap();
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut details = valid.clone();
            f(&mut details[2]);
            SessionCeiling::parse(&details)
        };
        assert!(mutate(&|v| v["type"] = "other".into()).is_err());
        assert!(mutate(&|v| v["scope"] = "rise.dev/Organization/default".into()).is_err());
        assert!(mutate(&|v| v["labelSelector"]["key"] = "team".into()).is_err());
        assert!(
            mutate(&|v| v["permissions"][0]["kinds"] = serde_json::json!(["rise.dev/Widget"]))
                .is_err()
        );
        assert!(mutate(&|v| v["permissions"][0]["verbs"] = serde_json::json!([])).is_err());
        assert!(mutate(&|v| v["extra"] = true.into()).is_err());
        assert!(SessionCeiling::parse(&[]).is_err());
    }

    #[test]
    fn ceiling_round_trips_to_the_request() {
        let request = AccessRequest::Restricted {
            grants: vec![
                grant("app", Some("staging"), Preset::Develop),
                Grant {
                    project: "app".into(),
                    environment: None,
                    preset: None,
                    permissions: vec![Permission::View, Permission::Logs],
                },
                grant("app", None, Preset::Read),
            ],
        };
        let c = SessionCeiling::parse(&compile(ORG, &request).unwrap()).unwrap();
        assert_eq!(c.to_access(), request.normalized());
    }
}
