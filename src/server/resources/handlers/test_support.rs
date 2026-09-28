//! Shared scaffolding for `dispatch_tests` and `conformance`.
//!
//! Both drive the generic resource API through the `dispatch_*_inner`
//! functions against a real Postgres-backed `ResourceApiCtx` — the same code
//! path the four Axum handlers run, minus only the `State`/`Path`/`Query`/
//! `Json` extraction. Everything here is `pub(super)`, reachable by both
//! sibling test modules through `use super::test_support::*;`.

use super::*;
use crate::db::models::User;
use crate::server::auth::identity::resolve_identity;
pub(super) use crate::server::auth::user_identity::UserPrincipal;
use rise_backend_auth::{IdentityClaims, RiseToken};
use rise_resource_api::RESOURCE_DEFINITION_KIND;
use rise_resource_store_postgres::PgResourceStore;
use serde_json::{json, Value};

pub(super) const OPERATOR: &str = "operator@example.com";
pub(super) const PLAIN_USER: &str = "plain-user@example.com";
/// The configured default Organization's name, matching
/// `default_default_organization_name()` in `settings.rs`. Used to
/// exercise the policy-audit `OrganizationWithoutAdmin` severity downgrade
/// (decision D4), which turns on this exact name.
pub(super) const DEFAULT_ORGANIZATION: &str = "default";

/// Build a `ResourceApiCtx` over a real `PgResourceStore`, with `OPERATOR`
/// on the operator email allowlist. The resource store schema is layered on
/// top of the root migrations `#[sqlx::test]` already ran, and the baseline
/// policy is seeded exactly as startup seeds it — without it an operator
/// would still reach everything (the evaluator hardcodes that), but nothing
/// else in the model would be present to reason about.
pub(super) async fn ctx(pool: sqlx::PgPool) -> ResourceApiCtx {
    ctx_with_operators(pool, vec![OPERATOR.into()], vec![]).await
}

pub(super) async fn ctx_with_operators(
    pool: sqlx::PgPool,
    operator_users: Vec<String>,
    operator_idp_groups: Vec<String>,
) -> ResourceApiCtx {
    rise_resource_store_postgres::run_migrations(&pool)
        .await
        .expect("resource store migrations");
    let pg_store = Arc::new(PgResourceStore::new(pool.clone()));
    let store: Arc<dyn ResourceStore> = pg_store.clone();
    crate::server::policy_seed::run(store.as_ref())
        .await
        .expect("seed baseline policy");
    ResourceApiCtx {
        store,
        authz: ResourceAuthorizer::new(
            pg_store,
            pool.clone(),
            crate::server::authz::OperatorSelectors {
                users: Arc::new(operator_users),
                idp_groups: Arc::new(operator_idp_groups),
            },
        ),
        tokens: Arc::new(token::tests::service(pool)),
        default_organization_name: DEFAULT_ORGANIZATION.to_string(),
    }
}

/// A `UserPrincipal` naming a fresh User resource that does not exist.
pub(super) fn test_user_principal() -> UserPrincipal {
    let uid = Uuid::new_v4();
    UserPrincipal {
        name: format!("u-{}", uid.simple()),
        uid,
    }
}

/// An `AnyAuth` carrying a User session. Neither the typed `users` row nor
/// the `User` resource the session names need exist — operator standing
/// comes from the email allowlist, and a User resource matters only for
/// Group ties and bindings naming it (see [`create_user_principal`]).
pub(super) fn auth(email: &str) -> AnyAuth {
    user_session(email, test_user_principal())
}

/// An `AnyAuth` for a session naming `principal`.
pub(super) fn user_session(email: &str, principal: UserPrincipal) -> AnyAuth {
    AnyAuth::User(AuthContext::User(
        User {
            id: Uuid::new_v4(),
            email: email.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        Some(principal),
    ))
}

/// An `AnyAuth` carrying a controller token with the given controller id.
pub(super) fn any_controller(id: &str) -> AnyAuth {
    AnyAuth::Controller(ControllerAuthContext(
        crate::server::auth::controller::ControllerPrincipal {
            name: id.to_string(),
            uid: Uuid::new_v4(),
        },
    ))
}

/// Create a live `Controller` resource named `name`, as the operator.
pub(super) async fn create_controller(ctx: &ResourceApiCtx, name: &str) -> Value {
    create_at(
        ctx,
        "rise.dev/v1alpha1/controllers",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "Controller",
            "metadata": {"name": name},
            "spec": {},
        }),
    )
    .await
}

/// Create a live Controller named `name` and grant `controller:<name>` the
/// given statements via a seeded `PlatformRole` + `PlatformRoleBinding`,
/// mirroring `grant_authenticated` but naming the Controller subject
/// directly — a `PlatformRoleBinding`'s `controller:` subject must
/// identify a live Controller (admission-checked), and an org
/// `RoleBinding` never reaches a Controller anyway (ADR-0001 §3), so this
/// is always a platform grant.
pub(super) async fn grant_controller(ctx: &ResourceApiCtx, name: &str, statements: Value) {
    create_controller(ctx, name).await;
    let role_name = format!("{name}-role");
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformroles",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": role_name},
            "spec": {"statements": statements},
        }),
    )
    .await;
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformrolebindings",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRoleBinding",
            "metadata": {"name": format!("{name}-binding")},
            "spec": {
                "subject": format!("controller:{name}"),
                "roleRef": {"kind": "PlatformRole", "name": role_name},
            },
        }),
    )
    .await;
}

/// Read a `Response` into `(status, json_body)`.
pub(super) async fn read(resp: Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("response body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// Register a root-scoped `widgets` collection (group `example.dev`) served
/// at both `v1` and `v2`, with `v1` as the storage version.
pub(super) async fn register_widget_rd(ctx: &ResourceApiCtx) {
    let spec = json!({
        "group": "example.dev",
        "kind": "Widget",
        "plural": "widgets",
        "versions": [
            {"name": "v1", "served": true, "storage": true},
            {"name": "v2", "served": true, "storage": false},
        ],
    });
    ctx.store
        .register_resource_definition(CreateResourceParams {
            labels: Default::default(),
            api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "widgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .expect("register widgets RD");
}

/// Register an Organization-scoped `gadgets` collection whose declared
/// parent is the built-in `rise.dev/v1alpha1` `Organization` (depth 1).
pub(super) async fn register_gadget_rd(ctx: &ResourceApiCtx) {
    let spec = json!({
        "group": "example.dev",
        "kind": "Gadget",
        "plural": "gadgets",
        "parent": {"apiVersion": "rise.dev/v1alpha1", "kind": "Organization"},
        "versions": [{"name": "v1", "served": true, "storage": true}],
    });
    ctx.store
        .register_resource_definition(CreateResourceParams {
            labels: Default::default(),
            api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "gadgets.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .expect("register gadgets RD");
}

/// Register a `gizmos` collection whose declared parent is the `Gadget`
/// collection — a depth-2 chain (`Gizmo` → `Gadget` → `Organization`).
pub(super) async fn register_gizmo_rd(ctx: &ResourceApiCtx) {
    let spec = json!({
        "group": "example.dev",
        "kind": "Gizmo",
        "plural": "gizmos",
        "parent": {"apiVersion": "example.dev/v1", "kind": "Gadget"},
        "versions": [{"name": "v1", "served": true, "storage": true}],
    });
    ctx.store
        .register_resource_definition(CreateResourceParams {
            labels: Default::default(),
            api_version: rise_resource_api::API_VERSION_V1ALPHA1.to_string(),
            kind: RESOURCE_DEFINITION_KIND.to_string(),
            name: "gizmos.example.dev".to_string(),
            parent_uid: None,
            annotations: BTreeMap::new(),
            finalizers: vec![],
            owner_references: vec![],
            spec,
            validator: None,
        })
        .await
        .expect("register gizmos RD");
}

/// POST `body` to `path`, asserting a 201, and return the created JSON.
pub(super) async fn create_at(ctx: &ResourceApiCtx, path: &str, body: Value) -> Value {
    let resp = dispatch_post_inner(ctx, path.to_string(), auth(OPERATOR), body)
        .await
        .expect("create resource");
    let (status, created) = read(resp).await;
    assert_eq!(status, StatusCode::CREATED, "unexpected create status");
    created
}

/// Parse `metadata.uid` from a resource JSON body.
pub(super) fn uid_of(resource: &Value) -> Uuid {
    resource["metadata"]["uid"]
        .as_str()
        .expect("uid")
        .parse()
        .expect("parse uid")
}

/// Create an Organization through the generic API.
pub(super) async fn create_org(ctx: &ResourceApiCtx, name: &str) -> Value {
    create_at(
        ctx,
        "rise.dev/v1alpha1/organizations",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "Organization",
            "metadata": {"name": name},
            "spec": {"displayName": name},
        }),
    )
    .await
}

/// Create an Organization bootstrapped with `admin_subject` as its first
/// org-admin, in the same atomic create as the Organization itself
/// (ADR-0001 §5) — the shape `bootstrap_admin_creates_org_and_binding_atomically`
/// exercises directly.
pub(super) async fn create_org_with_admin(
    ctx: &ResourceApiCtx,
    name: &str,
    admin_subject: &str,
) -> Value {
    create_at(
        ctx,
        "rise.dev/v1alpha1/organizations",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "Organization",
            "metadata": {"name": name},
            "spec": {"displayName": name},
            "bootstrap": {"admin": admin_subject},
        }),
    )
    .await
}

/// Create a live root `User` resource and an `AnyAuth` for a session naming
/// it, as a login resolving to that User would produce. Returns the subject
/// string (`user:<name>`) alongside the auth.
pub(super) async fn create_user_principal(ctx: &ResourceApiCtx) -> (String, AnyAuth) {
    let name = test_user_principal().name;
    let created = create_at(
        ctx,
        "rise.dev/v1alpha1/users",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "User",
            "metadata": {"name": name},
            "spec": {},
        }),
    )
    .await;
    let auth = user_session(
        &format!("{name}@example.com"),
        UserPrincipal {
            name: name.clone(),
            uid: uid_of(&created),
        },
    );
    (format!("user:{name}"), auth)
}

/// Grant every authenticated caller `statements`, platform-wide.
///
/// One `PlatformRole` plus one `PlatformRoleBinding` on
/// `system:authenticated` at wildcard scope — the only shape that reaches an
/// ordinary principal today, since a binding naming a `user:` subject needs
/// a `User` resource and those go live with identity resolution.
pub(super) async fn grant_authenticated(ctx: &ResourceApiCtx, name: &str, statements: Value) {
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformroles",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": name},
            "spec": {"statements": statements},
        }),
    )
    .await;
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformrolebindings",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRoleBinding",
            "metadata": {"name": name},
            "spec": {
                "subject": "system:authenticated",
                "roleRef": {"kind": "PlatformRole", "name": name},
            },
        }),
    )
    .await;
}

/// Deny `statements` platform-wide at `scope`, via `system:authenticated`
/// (a platform ceiling, ADR-0001 §5) — the shape used to narrow an
/// org-admin's baseline within one Organization only.
pub(super) async fn platform_deny(
    ctx: &ResourceApiCtx,
    name: &str,
    scope: &str,
    statements: Value,
) {
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformroles",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": name},
            "spec": {"statements": statements},
        }),
    )
    .await;
    create_at(
        ctx,
        "rise.dev/v1alpha1/platformrolebindings",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRoleBinding",
            "metadata": {"name": name},
            "spec": {
                "subject": "system:authenticated",
                "scope": scope,
                "roleRef": {"kind": "PlatformRole", "name": name},
            },
        }),
    )
    .await;
}

/// Create an Organization-scoped `Group` named `name`.
pub(super) async fn create_group(ctx: &ResourceApiCtx, org: &str, name: &str) {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/groups/{org}"),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "Group",
            "metadata": {"name": name},
            "spec": {},
        }),
    )
    .await;
}

/// Add `user_name` to `group` via a `GroupMembership` marker, optionally
/// owned by `owner` (a User's resource UID) so a later delete of that User
/// collects this marker (ADR-0001 §1 — an unowned marker survives and
/// reactivates instead).
pub(super) async fn add_member(
    ctx: &ResourceApiCtx,
    org: &str,
    group: &str,
    user_name: &str,
    owner: Option<Uuid>,
) -> Value {
    let mut metadata = json!({"name": user_name});
    if let Some(uid) = owner {
        metadata["ownerReferences"] = json!([{
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "User",
            "name": user_name,
            "uid": uid,
        }]);
    }
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/groupmemberships/{org}/{group}"),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "GroupMembership",
            "metadata": metadata,
            "spec": {},
        }),
    )
    .await
}

/// Create an org-scoped `Role` under `org` carrying `statements`.
pub(super) async fn create_role(ctx: &ResourceApiCtx, org: &str, name: &str, statements: Value) {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/roles/{org}"),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "Role",
            "metadata": {"name": name},
            "spec": {"statements": statements},
        }),
    )
    .await;
}

/// Bind `subject` to `role_ref` (e.g. `{"kind": "Role", "name": ...}` or
/// `{"kind": "PlatformRole", "name": ...}`) via an org `RoleBinding` under
/// `org`, named `name`. Scope is omitted, so it defaults to the org itself
/// (ADR-0001 §5 scenario 4).
pub(super) async fn bind_role(
    ctx: &ResourceApiCtx,
    org: &str,
    name: &str,
    subject: &str,
    role_ref: Value,
) {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/rolebindings/{org}"),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "RoleBinding",
            "metadata": {"name": name},
            "spec": {
                "subject": subject,
                "roleRef": role_ref,
            },
        }),
    )
    .await;
}

/// Create a `Gadget` under `org` — an Organization-scoped example collection
/// (needs `register_gadget_rd` to have run first).
pub(super) async fn create_gadget(ctx: &ResourceApiCtx, org: &str, name: &str) -> Value {
    create_at(
        ctx,
        &format!("example.dev/v1/gadgets/{org}"),
        json!({
            "apiVersion": "example.dev/v1",
            "kind": "Gadget",
            "metadata": {"name": name},
            "spec": {},
        }),
    )
    .await
}

/// JSON body for creating a widget at the given served apiVersion.
pub(super) fn widget_body(api_version: &str, name: &str) -> Value {
    json!({
        "apiVersion": api_version,
        "kind": "Widget",
        "metadata": {"name": name},
        "spec": {"size": "large"},
    })
}

/// POST a widget and return the created resource JSON.
pub(super) async fn create_widget(ctx: &ResourceApiCtx, api_version: &str, name: &str) -> Value {
    let resp = dispatch_post_inner(
        ctx,
        "example.dev/v1/widgets".to_string(),
        auth(OPERATOR),
        widget_body(api_version, name),
    )
    .await
    .expect("create widget");
    let (status, body) = read(resp).await;
    assert_eq!(status, StatusCode::CREATED, "unexpected create status");
    body
}

/// A root-scoped Widget carrying `rise.dev/owner`, for policy-audit tests
/// that need the seeded `resource-owner` binding's `labelSelector` to
/// match *something* — otherwise every fresh install carries its own
/// `selectorMatchesNothing` finding for that binding, which would make
/// exact finding-count assertions depend on unrelated seed behavior. A
/// root-scoped setter is deliberate: naming a `user:` subject on one
/// produces no `inertOwnerLabel` finding of its own (the seeded binding's
/// `ResourceOrganization` clamp has no organization to compare against).
pub(super) async fn create_widget_with_owner_label(ctx: &ResourceApiCtx, name: &str) -> Value {
    create_at(
        ctx,
        "example.dev/v1/widgets",
        json!({
            "apiVersion": "example.dev/v1",
            "kind": "Widget",
            "metadata": {"name": name, "labels": {"rise.dev/owner": "user:nobody"}},
            "spec": {"size": "large"},
        }),
    )
    .await
}

/// A resource's stored UID, read as the operator.
pub(super) async fn resource_uid(ctx: &ResourceApiCtx, path: &str) -> Uuid {
    let (status, body) = read(
        dispatch_get_inner(
            ctx,
            path.to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get for uid"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected get status for {path}");
    uid_of(&body)
}

/// GET `path` as `auth`.
pub(super) async fn get_as(
    ctx: &ResourceApiCtx,
    path: &str,
    auth: AnyAuth,
) -> Result<Response, ServerError> {
    dispatch_get_inner(ctx, path.to_string(), auth, PendingDeletionQuery::default()).await
}

/// The status code either side of a dispatch result carries.
pub(super) fn status_of(result: Result<Response, ServerError>) -> StatusCode {
    match result {
        Ok(resp) => resp.status(),
        Err(err) => err.status,
    }
}

/// Whether `auth` may perform `verb` on the live resource `uid`, evaluated
/// through the same `AuthorizationContext` API the request path uses
/// (`read_context` → `tree` → `allows`).
pub(super) async fn allowed(ctx: &ResourceApiCtx, auth: &AnyAuth, uid: Uuid, verb: Verb) -> bool {
    let authz_ctx = ctx.authz.read_context(auth).await.expect("read context");
    let tree = authz_ctx.tree(uid).await.expect("resource tree");
    authz_ctx.allows(&tree, verb, None).await.expect("allows")
}

/// The bare name half of a `user:<name>` subject.
pub(super) fn user_name(subject: &str) -> &str {
    subject.strip_prefix("user:").expect("user subject")
}

// -----------------------------------------------------------------------------
// The token subresource (ADR-0001 §7)
// -----------------------------------------------------------------------------

pub(super) const IP: &str = "203.0.113.7";

pub(super) fn identity_body(name: &str, kind: &str) -> Value {
    json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": kind,
        "metadata": {"name": name},
        "spec": {},
    })
}

pub(super) async fn create_service_account(ctx: &ResourceApiCtx, org: &str, name: &str) -> Value {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/serviceaccounts/{org}"),
        identity_body(name, "ServiceAccount"),
    )
    .await
}

pub(super) fn trust_policy_body(kind: &str, name: &str, issuer: &str, claims: Value) -> Value {
    json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": kind,
        "metadata": {"name": name},
        "spec": {"issuer": issuer, "claims": claims},
    })
}

pub(super) async fn trust_service_account(
    ctx: &ResourceApiCtx,
    org: &str,
    sa: &str,
    name: &str,
    issuer: &str,
    claims: Value,
) {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/serviceaccounttrustpolicies/{org}/{sa}"),
        trust_policy_body("ServiceAccountTrustPolicy", name, issuer, claims),
    )
    .await;
}

pub(super) async fn trust_controller(
    ctx: &ResourceApiCtx,
    controller: &str,
    name: &str,
    issuer: &str,
    claims: Value,
) {
    create_at(
        ctx,
        &format!("rise.dev/v1alpha1/controllertrustpolicies/{controller}"),
        trust_policy_body("ControllerTrustPolicy", name, issuer, claims),
    )
    .await;
}

pub(super) fn exchange_body(assertion: &str) -> Value {
    json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
        "subject_token": assertion,
        "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
    })
}

/// A credential-less POST: the workload exchange path.
pub(super) async fn exchange(
    ctx: &ResourceApiCtx,
    path: &str,
    body: Value,
) -> Result<Response, ServerError> {
    dispatch_post_any(ctx, path.to_string(), MaybeAuth(None), body, IP).await
}

/// An authenticated POST: the delegated path, or an ordinary create.
pub(super) async fn post_as(
    ctx: &ResourceApiCtx,
    path: &str,
    auth: AnyAuth,
    body: Value,
) -> Result<Response, ServerError> {
    dispatch_post_any(ctx, path.to_string(), MaybeAuth(Some(auth)), body, IP).await
}

/// Decode a minted token through the service's own verifier.
pub(super) fn decode(ctx: &ResourceApiCtx, body: &Value) -> IdentityClaims {
    let token = body["access_token"].as_str().expect("access_token");
    match ctx
        .tokens
        .signer()
        .verify_rise_jwt(token)
        .expect("verifies")
    {
        RiseToken::Identity(claims) => claims,
        other => panic!("expected an identity token, got {other:?}"),
    }
}

/// Authenticate a minted token exactly as the middleware does.
pub(super) async fn identity_auth(ctx: &ResourceApiCtx, body: &Value) -> AnyAuth {
    let claims = decode(ctx, body);
    AnyAuth::User(AuthContext::Identity(
        resolve_identity(ctx.store.as_ref(), &claims, token::tests::RISE_URL)
            .await
            .expect("the minted token resolves to a live principal"),
    ))
}

pub(super) const SA_TOKEN: &str = "rise.dev/v1alpha1/serviceaccounts/acme/ci/token";
pub(super) const CI_CLAIMS: &str = "repo:acme/app:ref:main";
