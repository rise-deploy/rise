//! ADR-0001 appendix conformance scenarios (Tier 2), driven through
//! `dispatch_*_inner` against Postgres — the same code path
//! `super::dispatch_tests` exercises, sharing its scaffolding via
//! `super::test_support`.
//!
//! No per-subject token count cap and no revocation list exist (ADR-0001
//! L1048-1050 accepts this), so none is asserted here. Denials are 404 when
//! the caller lacks `get` on the target (masked) and 403 when `get` is held
//! but the verb in question is not.

use super::test_support::*;
use super::*;
use crate::server::auth::identity::{resolve_identity, IdentityRejection};
use rise_backend_auth::MAX_DELEGATION_DEPTH;
use serde_json::json;

/// ADR-0001 scenario 23
/// ADR-0001 scenario 13
/// ADR-0001 scenario 25
///
/// One User bootstrapped as admin of two Organizations ignores each org's
/// own Deny only inside that org, and each org's distinct platform ceiling
/// still narrows her independently — administering acme does not administer
/// beta, and a foreign group tie grants nothing at all.
#[sqlx::test]
async fn a_user_administers_two_organizations_independently(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    register_gadget_rd(&ctx).await;
    let (alice_subject, alice_auth) = create_user_principal(&ctx).await;
    create_org_with_admin(&ctx, "acme", &alice_subject).await;
    create_org_with_admin(&ctx, "beta", &alice_subject).await;
    let ga = create_gadget(&ctx, "acme", "g").await;
    let gb = create_gadget(&ctx, "beta", "g").await;

    create_role(
        &ctx,
        "acme",
        "org-cap",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["delete"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "org-cap-binding",
        &alice_subject,
        json!({"kind": "Role", "name": "org-cap"}),
    )
    .await;

    create_role(
        &ctx,
        "beta",
        "org-cap",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["update"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "beta",
        "org-cap-binding",
        "system:authenticated",
        json!({"kind": "Role", "name": "org-cap"}),
    )
    .await;

    platform_deny(
        &ctx,
        "acme-ceiling",
        "rise.dev/Organization/acme",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["update"]}]),
    )
    .await;
    platform_deny(
        &ctx,
        "beta-ceiling",
        "rise.dev/Organization/beta",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["delete"]}]),
    )
    .await;

    // The org-level Deny is exempted for its own admin; the platform ceiling
    // is not — and each org's ceiling stays put in the other.
    assert!(allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Delete).await);
    assert!(!allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Update).await);
    assert!(allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Update).await);
    assert!(!allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Delete).await);

    assert_eq!(
        status_of(
            dispatch_delete_inner(
                &ctx,
                "example.dev/v1/gadgets/beta/g".to_string(),
                alice_auth.clone(),
            )
            .await
        ),
        StatusCode::FORBIDDEN
    );
    let resp = dispatch_put_inner(
        &ctx,
        "example.dev/v1/gadgets/beta/g".to_string(),
        alice_auth.clone(),
        json!({
            "apiVersion": "example.dev/v1",
            "kind": "Gadget",
            "metadata": {"name": "g", "revision": gb["metadata"]["revision"]},
            "spec": {},
        }),
    )
    .await
    .expect("alice's org-admin baseline reaches beta's update");
    assert_eq!(resp.status(), StatusCode::OK);

    // A foreign member: ordinary access in his own org, nothing in the other.
    let (bob_subject, bob_auth) = create_user_principal(&ctx).await;
    create_group(&ctx, "beta", "devs").await;
    add_member(&ctx, "beta", "devs", user_name(&bob_subject), None).await;
    create_role(
        &ctx,
        "beta",
        "devs-cap",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get", "update", "delete"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "beta",
        "devs-binding",
        "group:beta/devs",
        json!({"kind": "Role", "name": "devs-cap"}),
    )
    .await;

    assert!(allowed(&ctx, &bob_auth, uid_of(&gb), Verb::Get).await);
    assert!(!allowed(&ctx, &bob_auth, uid_of(&gb), Verb::Update).await);
    assert!(!allowed(&ctx, &bob_auth, uid_of(&ga), Verb::Get).await);

    let resp = dispatch_delete_inner(
        &ctx,
        "example.dev/v1/gadgets/acme/g".to_string(),
        alice_auth,
    )
    .await
    .expect("acme's org Deny on delete is exempted for its own admin");
    assert_eq!(resp.status(), StatusCode::OK);
}

/// ADR-0001 scenario 25
///
/// Editing the shipped `PlatformRole/org-admin` baseline moves every
/// Organization's admin standing at once, including a Deny authored into the
/// baseline itself (exempted for admins, same as any other org-tier Deny);
/// a platform ceiling scoped to one Organization narrows only that one.
#[sqlx::test]
async fn editing_the_org_admin_baseline_moves_every_org_while_a_scoped_deny_moves_one(
    pool: sqlx::PgPool,
) {
    let ctx = ctx(pool).await;
    register_gadget_rd(&ctx).await;
    let (alice_subject, alice_auth) = create_user_principal(&ctx).await;
    create_org_with_admin(&ctx, "acme", &alice_subject).await;
    create_org_with_admin(&ctx, "beta", &alice_subject).await;
    let ga = create_gadget(&ctx, "acme", "g").await;
    let gb = create_gadget(&ctx, "beta", "g").await;

    let (status, org_admin) = read(
        dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/platformroles/org-admin".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get the seeded org-admin role"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Narrow the baseline: everything but `delete`.
    let (status, narrowed) = read(
        dispatch_put_inner(
            &ctx,
            "rise.dev/v1alpha1/platformroles/org-admin".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRole",
                "metadata": {"name": "org-admin", "revision": org_admin["metadata"]["revision"]},
                "spec": {"statements": [{
                    "effect": "Allow",
                    "kinds": "*",
                    "verbs": ["get", "list", "create", "update", "use"],
                }]},
            }),
        )
        .await
        .expect("narrow the org-admin baseline"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(!allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Delete).await);
    assert!(!allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Delete).await);
    assert!(allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Update).await);
    assert!(allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Update).await);

    // Re-widen, but plant the Role's own Deny on update: it reaches admins
    // through the org tier and is exempted there, never substituting for a
    // platform ceiling.
    let (status, _) = read(
        dispatch_put_inner(
            &ctx,
            "rise.dev/v1alpha1/platformroles/org-admin".to_string(),
            auth(OPERATOR),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "PlatformRole",
                "metadata": {"name": "org-admin", "revision": narrowed["metadata"]["revision"]},
                "spec": {"statements": [
                    {"effect": "Allow", "kinds": "*", "verbs": ["get", "list", "create", "update", "delete", "use"]},
                    {"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["update"]},
                ]},
            }),
        )
        .await
        .expect("re-widen the baseline with its own Deny"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Update).await);
    assert!(allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Update).await);

    // A platform ceiling scoped to acme moves acme only.
    platform_deny(
        &ctx,
        "acme-ceiling",
        "rise.dev/Organization/acme",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["update"]}]),
    )
    .await;

    assert!(!allowed(&ctx, &alice_auth, uid_of(&ga), Verb::Update).await);
    assert!(allowed(&ctx, &alice_auth, uid_of(&gb), Verb::Update).await);
}

/// ADR-0001 scenario 16
/// ADR-0001 scenario 27
/// ADR-0001 scenario 10
///
/// Removing a User from a Group revokes Group-derived access on the next
/// request, re-adding restores it, and deactivating the User revokes
/// everything the name reaches until it is reactivated. Login and session
/// resolution for scenario 10 are covered in `auth::user_identity`.
#[sqlx::test]
async fn membership_removal_and_deactivation_revoke_group_access_on_the_next_request(
    pool: sqlx::PgPool,
) {
    let ctx = ctx(pool).await;
    register_gadget_rd(&ctx).await;
    create_org(&ctx, "acme").await;
    let (bob_subject, bob_auth) = create_user_principal(&ctx).await;
    let bob_name = user_name(&bob_subject).to_string();
    create_group(&ctx, "acme", "devs").await;
    add_member(&ctx, "acme", "devs", &bob_name, None).await;
    create_role(
        &ctx,
        "acme",
        "devs-cap",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get", "list"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "devs-binding",
        "group:acme/devs",
        json!({"kind": "Role", "name": "devs-cap"}),
    )
    .await;
    create_gadget(&ctx, "acme", "g").await;
    let gadget_path = "example.dev/v1/gadgets/acme/g".to_string();

    assert_eq!(
        status_of(get_as(&ctx, &gadget_path, bob_auth.clone()).await),
        StatusCode::OK
    );

    let resp = dispatch_delete_inner(
        &ctx,
        format!("rise.dev/v1alpha1/groupmemberships/acme/devs/{bob_name}"),
        auth(OPERATOR),
    )
    .await
    .expect("operator removes the membership");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        status_of(get_as(&ctx, &gadget_path, bob_auth.clone()).await),
        StatusCode::NOT_FOUND
    );

    add_member(&ctx, "acme", "devs", &bob_name, None).await;
    assert_eq!(
        status_of(get_as(&ctx, &gadget_path, bob_auth.clone()).await),
        StatusCode::OK
    );

    let (status, bob_resource) = read(
        dispatch_get_inner(
            &ctx,
            format!("rise.dev/v1alpha1/users/{bob_name}"),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get bob"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, deactivated) = read(
        dispatch_put_inner(
            &ctx,
            format!("rise.dev/v1alpha1/users/{bob_name}"),
            auth(OPERATOR),
            json!({
                "apiVersion": "rise.dev/v1alpha1",
                "kind": "User",
                "metadata": {"name": bob_name, "revision": bob_resource["metadata"]["revision"]},
                "spec": {"active": false},
            }),
        )
        .await
        .expect("deactivate bob"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        status_of(get_as(&ctx, &gadget_path, bob_auth.clone()).await),
        StatusCode::NOT_FOUND
    );

    let resp = dispatch_put_inner(
        &ctx,
        format!("rise.dev/v1alpha1/users/{bob_name}"),
        auth(OPERATOR),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "User",
            "metadata": {"name": bob_name, "revision": deactivated["metadata"]["revision"]},
            "spec": {"active": true},
        }),
    )
    .await
    .expect("reactivate bob");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        status_of(get_as(&ctx, &gadget_path, bob_auth).await),
        StatusCode::OK
    );
}

/// ADR-0001 scenario 27
///
/// Removing the org-admin's last qualifying binding revokes admin standing
/// live; an operator recovers it with a fresh binding of the same shape;
/// and once the User also holds an ordinary Group tie, revoking admin again
/// leaves the ordinary grant in place while the org's own Deny now applies.
#[sqlx::test]
async fn admin_removal_is_live_and_an_operator_can_reappoint(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    register_gadget_rd(&ctx).await;
    let (alice_subject, alice_auth) = create_user_principal(&ctx).await;
    let alice_name = user_name(&alice_subject).to_string();
    create_org_with_admin(&ctx, "acme", &alice_subject).await;
    let g = create_gadget(&ctx, "acme", "g").await;
    let g_uid = uid_of(&g);

    create_role(
        &ctx,
        "acme",
        "org-cap",
        json!([{"effect": "Deny", "kinds": ["example.dev/Gadget"], "verbs": ["delete"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "org-cap-binding",
        "system:authenticated",
        json!({"kind": "Role", "name": "org-cap"}),
    )
    .await;

    assert!(allowed(&ctx, &alice_auth, g_uid, Verb::Delete).await);
    assert!(allowed(&ctx, &alice_auth, g_uid, Verb::Get).await);

    let binding_name = format!("org-admin-{alice_name}");
    let resp = dispatch_delete_inner(
        &ctx,
        format!("rise.dev/v1alpha1/rolebindings/acme/{binding_name}"),
        auth(OPERATOR),
    )
    .await
    .expect("operator removes the bootstrapped admin binding");
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(!allowed(&ctx, &alice_auth, g_uid, Verb::Get).await);
    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/gadgets/acme/g", alice_auth.clone()).await),
        StatusCode::NOT_FOUND
    );

    bind_role(
        &ctx,
        "acme",
        "admin-again",
        &alice_subject,
        json!({"kind": "PlatformRole", "name": ORG_ADMIN_PLATFORM_ROLE}),
    )
    .await;
    assert!(allowed(&ctx, &alice_auth, g_uid, Verb::Get).await);
    assert!(allowed(&ctx, &alice_auth, g_uid, Verb::Delete).await);

    create_group(&ctx, "acme", "devs").await;
    add_member(&ctx, "acme", "devs", &alice_name, None).await;
    create_role(
        &ctx,
        "acme",
        "reader",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "reader-binding",
        "group:acme/devs",
        json!({"kind": "Role", "name": "reader"}),
    )
    .await;

    let resp = dispatch_delete_inner(
        &ctx,
        "rise.dev/v1alpha1/rolebindings/acme/admin-again".to_string(),
        auth(OPERATOR),
    )
    .await
    .expect("operator removes the re-appointed admin binding");
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(allowed(&ctx, &alice_auth, g_uid, Verb::Get).await);
    assert!(!allowed(&ctx, &alice_auth, g_uid, Verb::Delete).await);
}

/// ADR-0001 scenario 16
/// ADR-0001 scenario 37
///
/// Deleting a User collects only the GroupMemberships that carry its
/// matching owner reference; an unowned marker survives and reactivates when
/// a User of the same canonical name is recreated, while a collected one
/// does not — and while no live User exists, neither grants anything, even
/// through the surviving row.
#[sqlx::test]
async fn deleting_a_user_collects_only_owner_referenced_memberships(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    register_gadget_rd(&ctx).await;
    create_org(&ctx, "acme").await;
    let (bob_subject, bob_auth) = create_user_principal(&ctx).await;
    let bob_name = user_name(&bob_subject).to_string();
    let bob_uid = resource_uid(&ctx, &format!("rise.dev/v1alpha1/users/{bob_name}")).await;

    create_group(&ctx, "acme", "devs").await;
    create_group(&ctx, "acme", "ops").await;
    let owned = add_member(&ctx, "acme", "devs", &bob_name, Some(bob_uid)).await;
    let marker = add_member(&ctx, "acme", "ops", &bob_name, None).await;
    create_role(
        &ctx,
        "acme",
        "devs-cap",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "devs-binding",
        "group:acme/devs",
        json!({"kind": "Role", "name": "devs-cap"}),
    )
    .await;
    create_role(
        &ctx,
        "acme",
        "ops-cap",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["list"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "ops-binding",
        "group:acme/ops",
        json!({"kind": "Role", "name": "ops-cap"}),
    )
    .await;
    create_gadget(&ctx, "acme", "g").await;

    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/gadgets/acme/g", bob_auth.clone()).await),
        StatusCode::OK
    );
    let resp = get_as(&ctx, "example.dev/v1/gadgets/acme", bob_auth.clone())
        .await
        .expect("list gadgets");
    let (status, listing) = read(resp).await;
    assert_eq!(status, StatusCode::OK);
    let items = listing["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert!(
        items[0].get("spec").is_some(),
        "get+list yields the full item"
    );

    ctx.store.delete(bob_uid).await.expect("delete bob");
    let owned_uid = uid_of(&owned);
    ctx.store
        .try_collect(owned_uid)
        .await
        .expect("collect the owned marker");
    assert!(
        ctx.store
            .ancestors(owned_uid)
            .await
            .expect("ancestors")
            .is_empty(),
        "the owned marker is gone"
    );
    assert!(
        !ctx.store
            .ancestors(uid_of(&marker))
            .await
            .expect("ancestors")
            .is_empty(),
        "the unowned marker survives"
    );

    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/gadgets/acme/g", bob_auth.clone()).await),
        StatusCode::NOT_FOUND
    );
    let resp = get_as(&ctx, "example.dev/v1/gadgets/acme", bob_auth.clone())
        .await
        .expect("list with no live User");
    let (_, listing) = read(resp).await;
    assert_eq!(listing["items"].as_array().map(Vec::len), Some(0));

    create_at(
        &ctx,
        "rise.dev/v1alpha1/users",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "User",
            "metadata": {"name": bob_name},
            "spec": {},
        }),
    )
    .await;

    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/gadgets/acme/g", bob_auth.clone()).await),
        StatusCode::NOT_FOUND
    );
    let resp = get_as(&ctx, "example.dev/v1/gadgets/acme", bob_auth)
        .await
        .expect("list after reactivation");
    let (_, listing) = read(resp).await;
    let items = listing["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert!(
        items[0].get("spec").is_none(),
        "list-only projection through the surviving marker"
    );
}

/// ADR-0001 scenario 7
/// ADR-0001 scenario 54
///
/// A Controller identity token dies with the Controller's UID exactly like a
/// ServiceAccount's, and policy narrowing or revoking its grant reaches it
/// live — a fresh Controller of the same name never revives the old claims.
#[sqlx::test]
async fn a_controller_token_dies_with_its_uid_and_policy_changes_reach_it_live(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    register_widget_rd(&ctx).await;
    grant_controller(
        &ctx,
        "k8s",
        json!([{"effect": "Allow", "kinds": ["example.dev/Widget"], "verbs": ["get"]}]),
    )
    .await;
    create_widget(&ctx, "example.dev/v1", "w").await;
    let k8s_uid = resource_uid(&ctx, "rise.dev/v1alpha1/controllers/k8s").await;

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/controllers/k8s/token",
        auth(OPERATOR),
        json!({}),
    )
    .await
    .expect("operator mints for the controller");
    let (_, token_body) = read(resp).await;
    let as_k8s = identity_auth(&ctx, &token_body).await;

    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/widgets/w", as_k8s.clone()).await),
        StatusCode::OK
    );

    let (status, role) = read(
        dispatch_get_inner(
            &ctx,
            "rise.dev/v1alpha1/platformroles/k8s-role".to_string(),
            auth(OPERATOR),
            PendingDeletionQuery::default(),
        )
        .await
        .expect("get k8s-role"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    dispatch_put_inner(
        &ctx,
        "rise.dev/v1alpha1/platformroles/k8s-role".to_string(),
        auth(OPERATOR),
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": "k8s-role", "revision": role["metadata"]["revision"]},
            "spec": {"statements": [
                {"effect": "Allow", "kinds": ["example.dev/Widget"], "verbs": ["list"]},
            ]},
        }),
    )
    .await
    .expect("narrow the controller's role");
    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/widgets/w", as_k8s.clone()).await),
        StatusCode::NOT_FOUND
    );

    let resp = dispatch_delete_inner(
        &ctx,
        "rise.dev/v1alpha1/platformrolebindings/k8s-binding".to_string(),
        auth(OPERATOR),
    )
    .await
    .expect("operator removes the controller's binding");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/widgets/w", as_k8s.clone()).await),
        StatusCode::NOT_FOUND
    );

    ctx.store
        .delete(k8s_uid)
        .await
        .expect("delete the controller");
    let claims = decode(&ctx, &token_body);
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &claims, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));
    let _ = ctx.store.try_collect(k8s_uid).await;

    let new_controller = create_controller(&ctx, "k8s").await;
    assert_ne!(uid_of(&new_controller), k8s_uid);
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &claims, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));

    // A fresh mint for the replacement resolves: it names the new, live UID.
    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/controllers/k8s/token",
        auth(OPERATOR),
        json!({}),
    )
    .await
    .expect("operator mints for the replacement controller");
    let (_, fresh_body) = read(resp).await;
    let fresh_claims = decode(&ctx, &fresh_body);
    assert!(
        resolve_identity(ctx.store.as_ref(), &fresh_claims, token::tests::RISE_URL)
            .await
            .is_ok()
    );
}

/// ADR-0001 scenario 54
/// ADR-0001 scenario 48
///
/// Revoking a delegator's own token-create grant stops it from minting the
/// next token, but every token it already minted keeps resolving — a token
/// is not a live capability query, only the identity is.
#[sqlx::test]
async fn revoking_token_create_stops_issuance_but_not_outstanding_tokens(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    create_org(&ctx, "acme").await;
    create_service_account(&ctx, "acme", "ci").await;
    create_controller(&ctx, "k8s").await;

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/controllers/k8s/token",
        auth(OPERATOR),
        json!({}),
    )
    .await
    .expect("operator mints for the controller");
    let (_, controller_token) = read(resp).await;
    let as_k8s = identity_auth(&ctx, &controller_token).await;

    create_at(
        &ctx,
        "rise.dev/v1alpha1/platformroles",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": "sa-token-minter"},
            "spec": {"statements": [{
                "effect": "Allow",
                "kinds": ["rise.dev/ServiceAccount"],
                "verbs": ["create"],
                "subresources": ["token"],
            }]},
        }),
    )
    .await;
    create_at(
        &ctx,
        "rise.dev/v1alpha1/platformrolebindings",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRoleBinding",
            "metadata": {"name": "k8s-mints-ci"},
            "spec": {
                "subject": "controller:k8s",
                "scope": "rise.dev/Organization/acme",
                "roleRef": {"kind": "PlatformRole", "name": "sa-token-minter"},
            },
        }),
    )
    .await;

    let resp = post_as(&ctx, SA_TOKEN, as_k8s.clone(), json!({}))
        .await
        .expect("the controller holds token-create on the ServiceAccount");
    let (status, sa_token) = read(resp).await;
    assert_eq!(status, StatusCode::OK, "{sa_token}");

    create_at(
        &ctx,
        "rise.dev/v1alpha1/platformroles",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRole",
            "metadata": {"name": "self-reader"},
            "spec": {"statements": [
                {"effect": "Allow", "kinds": ["rise.dev/ServiceAccount"], "verbs": ["get"]}
            ]},
        }),
    )
    .await;
    create_at(
        &ctx,
        "rise.dev/v1alpha1/platformrolebindings",
        json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "PlatformRoleBinding",
            "metadata": {"name": "ci-reads-itself"},
            "spec": {
                "subject": "serviceaccount:acme/ci",
                "scope": "rise.dev/ServiceAccount/acme/ci",
                "roleRef": {"kind": "PlatformRole", "name": "self-reader"},
            },
        }),
    )
    .await;

    let as_ci = identity_auth(&ctx, &sa_token).await;
    assert_eq!(
        status_of(
            get_as(
                &ctx,
                "rise.dev/v1alpha1/serviceaccounts/acme/ci",
                as_ci.clone()
            )
            .await
        ),
        StatusCode::OK
    );

    let resp = dispatch_delete_inner(
        &ctx,
        "rise.dev/v1alpha1/platformrolebindings/k8s-mints-ci".to_string(),
        auth(OPERATOR),
    )
    .await
    .expect("operator revokes the controller's minting grant");
    assert_eq!(resp.status(), StatusCode::OK);

    let err = post_as(&ctx, SA_TOKEN, as_k8s, json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.status, StatusCode::NOT_FOUND, "{}", err.message);

    assert_eq!(
        status_of(get_as(&ctx, "rise.dev/v1alpha1/serviceaccounts/acme/ci", as_ci).await),
        StatusCode::OK
    );
}

/// ADR-0001 scenario 7
/// ADR-0001 scenario 45
///
/// Authentication requires `sub` and `rise_uid` to name the same live
/// resource: a wrong UID, a wrong subject, a wrong-kind pairing, a wrong
/// audience, and an unsupported subject kind each fail — the last two on
/// their own distinct variant — and tombstoning the containing Organization
/// invalidates a token minted before the delete.
#[sqlx::test]
async fn an_identity_token_must_name_the_live_resource_it_was_minted_for(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    let org = create_org(&ctx, "acme").await;
    let org_uid = uid_of(&org);
    let ci = create_service_account(&ctx, "acme", "ci").await;
    let other = create_service_account(&ctx, "acme", "other").await;
    let k8s = create_controller(&ctx, "k8s").await;
    let ci_uid = uid_of(&ci);
    let other_uid = uid_of(&other);
    let k8s_uid = uid_of(&k8s);

    let resp = post_as(&ctx, SA_TOKEN, auth(OPERATOR), json!({}))
        .await
        .expect("mint for ci");
    let (_, body) = read(resp).await;
    let claims = decode(&ctx, &body);
    assert_eq!(claims.rise_uid, ci_uid);

    assert!(
        resolve_identity(ctx.store.as_ref(), &claims, token::tests::RISE_URL)
            .await
            .is_ok()
    );

    let mut wrong_uid = claims.clone();
    wrong_uid.rise_uid = other_uid;
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &wrong_uid, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));

    let mut wrong_sub = claims.clone();
    wrong_sub.sub = "serviceaccount:acme/other".to_string();
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &wrong_sub, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));

    let mut wrong_kind_uid = claims.clone();
    wrong_kind_uid.rise_uid = k8s_uid;
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &wrong_kind_uid, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));

    let mut wrong_kind_sub = claims.clone();
    wrong_kind_sub.sub = "controller:ci".to_string();
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &wrong_kind_sub, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));

    let mut wrong_aud = claims.clone();
    wrong_aud.aud = "https://someone-else.example.com".to_string();
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &wrong_aud, token::tests::RISE_URL).await,
        Err(IdentityRejection::Audience(_))
    ));

    let mut wrong_subject_kind = claims.clone();
    wrong_subject_kind.sub = "group:acme/devs".to_string();
    assert!(matches!(
        resolve_identity(
            ctx.store.as_ref(),
            &wrong_subject_kind,
            token::tests::RISE_URL
        )
        .await,
        Err(IdentityRejection::Subject(_))
    ));

    ctx.store.delete(org_uid).await.expect("delete the org");
    assert!(matches!(
        resolve_identity(ctx.store.as_ref(), &claims, token::tests::RISE_URL).await,
        Err(IdentityRejection::NoLiveResource)
    ));
}

/// ADR-0001 scenario 54
///
/// Every minted token respects the platform's max TTL regardless of what the
/// caller asks for: an over-long request clamps, an omitted one defaults to
/// the maximum, a shorter one is honored, and zero is refused outright.
#[sqlx::test]
async fn token_ttl_is_clamped_to_the_platform_maximum_at_the_route(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    create_org(&ctx, "acme").await;
    create_service_account(&ctx, "acme", "ci").await;

    let resp = post_as(
        &ctx,
        SA_TOKEN,
        auth(OPERATOR),
        json!({"expires_in": 99_999}),
    )
    .await
    .expect("clamp to the platform maximum");
    let (status, body) = read(resp).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["expires_in"], 600);
    let claims = decode(&ctx, &body);
    assert_eq!(claims.exp - claims.iat, 600);

    let resp = post_as(&ctx, SA_TOKEN, auth(OPERATOR), json!({}))
        .await
        .expect("an omitted expires_in defaults to the platform maximum");
    assert_eq!(read(resp).await.1["expires_in"], 600);

    let resp = post_as(&ctx, SA_TOKEN, auth(OPERATOR), json!({"expires_in": 30}))
        .await
        .expect("a shorter request is honored");
    assert_eq!(read(resp).await.1["expires_in"], 30);

    let err = post_as(&ctx, SA_TOKEN, auth(OPERATOR), json!({"expires_in": 0}))
        .await
        .unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST, "{}", err.message);
}

/// ADR-0001 scenario 48
///
/// A chain of delegated mints is bounded by the platform's delegation-depth
/// limit: the fourth hop lands exactly at the limit and records every prior
/// delegator, and the fifth is refused before any token is issued.
#[sqlx::test]
async fn delegation_stops_at_the_platform_depth_limit(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    create_org(&ctx, "acme").await;
    for name in ["s1", "s2", "s3", "s4", "s5"] {
        create_service_account(&ctx, "acme", name).await;
    }
    grant_authenticated(
        &ctx,
        "minter",
        json!([{
            "effect": "Allow",
            "kinds": ["rise.dev/ServiceAccount"],
            "verbs": ["create"],
            "subresources": ["token"],
        }]),
    )
    .await;

    let operator = auth(OPERATOR);
    let operator_id = operator.user_principal().unwrap().clone();

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/serviceaccounts/acme/s1/token",
        operator,
        json!({}),
    )
    .await
    .expect("operator mints s1");
    let (_, t1) = read(resp).await;
    let as_s1 = identity_auth(&ctx, &t1).await;

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/serviceaccounts/acme/s2/token",
        as_s1,
        json!({}),
    )
    .await
    .expect("s1 mints s2");
    let (_, t2) = read(resp).await;
    let as_s2 = identity_auth(&ctx, &t2).await;

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/serviceaccounts/acme/s3/token",
        as_s2,
        json!({}),
    )
    .await
    .expect("s2 mints s3");
    let (_, t3) = read(resp).await;
    let as_s3 = identity_auth(&ctx, &t3).await;

    let resp = post_as(
        &ctx,
        "rise.dev/v1alpha1/serviceaccounts/acme/s4/token",
        as_s3,
        json!({}),
    )
    .await
    .expect("s3 mints s4");
    let (_, t4) = read(resp).await;
    let claims4 = decode(&ctx, &t4);
    assert_eq!(MAX_DELEGATION_DEPTH, 4);
    assert_eq!(claims4.delegation_depth(), MAX_DELEGATION_DEPTH);
    let act3 = claims4.act.expect("act");
    assert_eq!(act3.sub, "serviceaccount:acme/s3");
    let act2 = act3.act.expect("act");
    assert_eq!(act2.sub, "serviceaccount:acme/s2");
    let act1 = act2.act.expect("act");
    assert_eq!(act1.sub, "serviceaccount:acme/s1");
    let act0 = act1.act.expect("act");
    assert_eq!(act0.sub, operator_id.subject().to_string());
    assert!(act0.act.is_none());

    let as_s4 = identity_auth(&ctx, &t4).await;
    let err = post_as(
        &ctx,
        "rise.dev/v1alpha1/serviceaccounts/acme/s5/token",
        as_s4,
        json!({}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, StatusCode::FORBIDDEN, "{}", err.message);
    assert!(
        err.message.contains("delegation chain would exceed"),
        "{}",
        err.message
    );
}

/// ADR-0001 scenario 33 (deterministic)
///
/// A grant and a revocation racing on the same Organization's policy cannot
/// both commit against stale facts: the revoke's own authorization reads the
/// row the grant inserted, so PostgreSQL's SERIALIZABLE isolation detects the
/// conflict at commit and the loser is told to retry from the beginning.
#[sqlx::test]
async fn a_grant_and_a_revocation_cannot_both_commit_on_stale_facts(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    let (w_subject, w_auth) = create_user_principal(&ctx).await;
    create_org_with_admin(&ctx, "acme", &w_subject).await;
    create_role(
        &ctx,
        "acme",
        "gadget-reader",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get"]}]),
    )
    .await;
    let binding_name = format!("org-admin-{}", user_name(&w_subject));
    let binding_path = format!("rise.dev/v1alpha1/rolebindings/acme/{binding_name}");

    let body = json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": "RoleBinding",
        "metadata": {"name": "delegated"},
        "spec": {
            "subject": "system:authenticated",
            "roleRef": {"kind": "Role", "name": "gadget-reader"},
        },
    });

    let attempt = ctx
        .authz
        .begin_write(&w_auth, 1)
        .await
        .expect("open the writer's attempt");
    let resp = create_once(
        &ctx,
        attempt.context(),
        "rise.dev/v1alpha1/rolebindings/acme",
        body.clone(),
    )
    .await
    .expect("the create is authorized inside the open attempt");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let revoke = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        dispatch_delete_inner(&ctx, binding_path.clone(), auth(OPERATOR)),
    )
    .await
    .expect("the revoke must not block on a row lock the open attempt does not hold");
    let revoke = revoke.expect("the revoke commits its own transaction");
    assert_eq!(revoke.status(), StatusCode::OK);

    let commit_err = attempt.commit().await.expect_err(
        "the revoke's authorization read the row the grant inserted, so replaying it is \
         required rather than letting both commit against stale facts",
    );
    assert!(commit_err.retryable, "{}", commit_err.message);

    let replay = dispatch_post_inner(
        &ctx,
        "rise.dev/v1alpha1/rolebindings/acme".to_string(),
        w_auth,
        body,
    )
    .await
    .expect_err("replaying from the beginning re-evaluates against the now-revoked admin");
    assert!(replay.status.is_client_error(), "{}", replay.message);

    assert_eq!(
        status_of(
            get_as(
                &ctx,
                "rise.dev/v1alpha1/rolebindings/acme/delegated",
                auth(OPERATOR),
            )
            .await
        ),
        StatusCode::NOT_FOUND
    );
}

/// ADR-0001 scenario 33 (stochastic)
///
/// Racing a grant against a revocation many times over never produces a
/// server error or a retryable error escaping the bounded retry loop, and
/// the stored result always matches what the grant's own outcome claims —
/// never a torn state where the binding both does and does not exist.
#[sqlx::test]
async fn concurrent_grants_and_revocations_never_leave_a_torn_result(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    let (w_subject, w_auth) = create_user_principal(&ctx).await;
    let mut granted = 0;
    let mut refused = 0;

    for i in 0..8 {
        let org = format!("o{i}");
        create_org_with_admin(&ctx, &org, &w_subject).await;
        create_role(
            &ctx,
            &org,
            "gadget-reader",
            json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get"]}]),
        )
        .await;
        let binding_name = format!("org-admin-{}", user_name(&w_subject));
        let binding_path = format!("rise.dev/v1alpha1/rolebindings/{org}/{binding_name}");
        let grant_path = format!("rise.dev/v1alpha1/rolebindings/{org}");
        let body = json!({
            "apiVersion": "rise.dev/v1alpha1",
            "kind": "RoleBinding",
            "metadata": {"name": "delegated"},
            "spec": {
                "subject": "system:authenticated",
                "roleRef": {"kind": "Role", "name": "gadget-reader"},
            },
        });

        let ctx_grant = ctx.clone();
        let w_for_grant = w_auth.clone();
        let grant = tokio::spawn(async move {
            dispatch_post_inner(&ctx_grant, grant_path, w_for_grant, body).await
        });
        let ctx_revoke = ctx.clone();
        let revoke = tokio::spawn(async move {
            dispatch_delete_inner(&ctx_revoke, binding_path, auth(OPERATOR)).await
        });

        let (grant_result, revoke_result) = tokio::join!(grant, revoke);
        let revoke_result = revoke_result.expect("revoke task");
        assert_eq!(
            revoke_result.expect("the revoke commits").status(),
            StatusCode::OK
        );

        let grant_result = grant_result.expect("grant task");
        let item_path = format!("rise.dev/v1alpha1/rolebindings/{org}/delegated");
        match grant_result {
            Ok(resp) => {
                assert_eq!(resp.status(), StatusCode::CREATED, "round {i} ({org})");
                granted += 1;
                assert_eq!(
                    status_of(get_as(&ctx, &item_path, auth(OPERATOR)).await),
                    StatusCode::OK,
                    "round {i} ({org}): a committed create must be visible to the next read"
                );
            }
            Err(err) => {
                assert!(
                    err.status.is_client_error(),
                    "round {i} ({org}): {}",
                    err.message
                );
                assert!(
                    !err.retryable,
                    "round {i} ({org}): a retryable error must never surface past the bounded \
                     retry loop"
                );
                refused += 1;
                assert_eq!(
                    status_of(get_as(&ctx, &item_path, auth(OPERATOR)).await),
                    StatusCode::NOT_FOUND,
                    "round {i} ({org})"
                );
            }
        }
    }

    println!(
        "concurrent_grants_and_revocations_never_leave_a_torn_result: \
         {granted} granted, {refused} refused of 8 rounds"
    );
}

/// ADR-0001 scenario 33 (stochastic)
/// ADR-0001 scenario 28
///
/// The same race, shaped as a GroupMembership create instead of a
/// RoleBinding: the revoker reads no GroupMembership predicate, so both
/// sides committing is a valid serial order, and the invariant is only that
/// neither a server error nor a stray retryable error ever escapes.
#[sqlx::test]
async fn a_membership_write_racing_an_admin_revocation_is_never_torn(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    let (w_subject, w_auth) = create_user_principal(&ctx).await;
    create_org_with_admin(&ctx, "acme", &w_subject).await;
    create_group(&ctx, "acme", "devs").await;
    create_role(
        &ctx,
        "acme",
        "gadget-reader",
        json!([{"effect": "Allow", "kinds": ["example.dev/Gadget"], "verbs": ["get"]}]),
    )
    .await;
    bind_role(
        &ctx,
        "acme",
        "devs-binding",
        "group:acme/devs",
        json!({"kind": "Role", "name": "gadget-reader"}),
    )
    .await;
    let (bob_subject, _bob_auth) = create_user_principal(&ctx).await;
    let bob_name = user_name(&bob_subject).to_string();

    let binding_name = format!("org-admin-{}", user_name(&w_subject));
    let binding_path = format!("rise.dev/v1alpha1/rolebindings/acme/{binding_name}");
    let membership_path = "rise.dev/v1alpha1/groupmemberships/acme/devs".to_string();
    let membership_body = json!({
        "apiVersion": "rise.dev/v1alpha1",
        "kind": "GroupMembership",
        "metadata": {"name": bob_name},
        "spec": {},
    });

    let ctx_grant = ctx.clone();
    let w_for_grant = w_auth;
    let grant = tokio::spawn(async move {
        dispatch_post_inner(&ctx_grant, membership_path, w_for_grant, membership_body).await
    });
    let ctx_revoke = ctx.clone();
    let revoke = tokio::spawn(async move {
        dispatch_delete_inner(&ctx_revoke, binding_path, auth(OPERATOR)).await
    });

    let (grant_result, revoke_result) = tokio::join!(grant, revoke);
    let revoke_result = revoke_result.expect("revoke task");
    assert_eq!(
        revoke_result.expect("the revoke commits").status(),
        StatusCode::OK
    );

    let grant_result = grant_result.expect("grant task");
    match grant_result {
        Ok(resp) => assert_eq!(resp.status(), StatusCode::CREATED),
        Err(err) => {
            assert!(err.status.is_client_error(), "{}", err.message);
            assert!(
                !err.retryable,
                "a retryable error must never surface past the bounded retry loop"
            );
        }
    }
}

/// ADR-0001 scenario 36
///
/// One request's `AuthorizationContext` memoizes what it read at the start:
/// a concurrent revocation does not change the answer for the rest of that
/// request, but the very next request re-reads and sees it.
#[sqlx::test]
async fn a_request_snapshot_is_local_to_the_request(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    register_widget_rd(&ctx).await;
    grant_authenticated(
        &ctx,
        "reader",
        json!([{"effect": "Allow", "kinds": ["example.dev/Widget"], "verbs": ["get"]}]),
    )
    .await;
    let w = create_widget(&ctx, "example.dev/v1", "w").await;
    let w_uid = uid_of(&w);

    let first = ctx
        .authz
        .read_context(&auth(PLAIN_USER))
        .await
        .expect("first snapshot");
    let tree = first.tree(w_uid).await.expect("resource tree");
    assert!(first.allows(&tree, Verb::Get, None).await.expect("allows"));

    let resp = dispatch_delete_inner(
        &ctx,
        "rise.dev/v1alpha1/platformrolebindings/reader".to_string(),
        auth(OPERATOR),
    )
    .await
    .expect("revoke the grant");
    assert_eq!(resp.status(), StatusCode::OK);

    // The memoized snapshot still answers from what it read at the start of
    // the request — a live re-read would see the binding gone.
    assert!(first.allows(&tree, Verb::Get, None).await.expect("allows"));

    let second = ctx
        .authz
        .read_context(&auth(PLAIN_USER))
        .await
        .expect("a fresh request re-reads");
    let tree2 = second.tree(w_uid).await.expect("resource tree");
    assert!(!second
        .allows(&tree2, Verb::Get, None)
        .await
        .expect("allows"));

    assert_eq!(
        status_of(get_as(&ctx, "example.dev/v1/widgets/w", auth(PLAIN_USER)).await),
        StatusCode::NOT_FOUND
    );
}

/// A session issued before identity resolution names no `User` resource. It
/// keeps the typed APIs until it expires, but the generic resource API cannot
/// tie it to a principal and refuses it — even for an allowlisted operator.
#[sqlx::test]
async fn a_legacy_session_naming_no_user_is_refused(pool: sqlx::PgPool) {
    let ctx = ctx(pool).await;
    let legacy = AnyAuth::User(AuthContext::User(
        crate::db::models::User {
            id: Uuid::new_v4(),
            email: OPERATOR.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        None,
    ));
    let err = get_as(&ctx, "rise.dev/v1alpha1/organizations", legacy)
        .await
        .unwrap_err();
    assert_eq!(err.status, StatusCode::UNAUTHORIZED, "{}", err.message);
}
