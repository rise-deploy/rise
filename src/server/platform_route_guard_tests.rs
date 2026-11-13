//! Every platform route refuses an external token that no service account of
//! the target project accepts.
//!
//! With `auth.allow_raw_external_tokens` on, `auth_middleware` admits any JWT
//! signed by an issuer some project's service account trusts, and
//! `platform_access_middleware` waves it through. Such a token can be minted by
//! an arbitrary third party when the issuer is public (a CI provider, say), so
//! the only thing between it and a platform handler is that handler resolving
//! the caller (`AuthContext::user` or `AuthContext::resolve_for_project`)
//! before it acts. This module drives such a token through every route
//! `platform_route_tree` registers and requires a 401/403/404 and no database
//! change from each.
//!
//! The route table below cannot silently drift from the router: the set of
//! `(method, path)` pairs is parsed from the route modules' source and must
//! equal the table, and the list of merged route modules is parsed from
//! `platform_route_tree` and must equal the modules the parser knows about.
//! Parsing source (rather than inspecting the built `Router`, which exposes no
//! route listing) keeps the check independent of the router it checks; a route
//! registered in a style the parser does not understand fails the test instead
//! of being skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use axum::{
    body::Body,
    extract::Request,
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use regex::Regex;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::db;
use crate::server::auth::browser_flow_tests::fixture;
use crate::server::auth::context::VerifiedExternalToken;
use crate::server::state::AppState;

/// Status of the router fallback in the harness. No handler produces it, so a
/// request answering with it never reached a registered route.
const NOT_ROUTED: StatusCode = StatusCode::IM_A_TEAPOT;

// ---------------------------------------------------------------------------
// Route discovery
// ---------------------------------------------------------------------------

/// A route module merged into the platform route tree: the expression
/// `platform_route_tree` merges, the source file defining it, and the function
/// in that file that builds the router.
struct RouteModule {
    merge_expr: &'static str,
    source: &'static str,
    function: &'static str,
}

const ROUTE_MODULES: &[RouteModule] = &[
    RouteModule {
        merge_expr: "auth::routes::platform_routes()",
        source: include_str!("auth/routes.rs"),
        function: "platform_routes",
    },
    RouteModule {
        merge_expr: "custom_domains::routes()",
        source: include_str!("custom_domains/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "project::routes::routes()",
        source: include_str!("project/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "team::routes::team_routes()",
        source: include_str!("team/routes.rs"),
        function: "team_routes",
    },
    RouteModule {
        merge_expr: "registry::routes::routes()",
        source: include_str!("registry/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "deployment::routes::deployment_routes()",
        source: include_str!("deployment/routes.rs"),
        function: "deployment_routes",
    },
    RouteModule {
        merge_expr: "service_accounts::routes::routes()",
        source: include_str!("service_accounts/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "env_vars::routes::routes()",
        source: include_str!("env_vars/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "environments::routes::routes()",
        source: include_str!("environments/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "extensions::routes::routes()",
        source: include_str!("extensions/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "encryption::routes::routes()",
        source: include_str!("encryption/routes.rs"),
        function: "routes",
    },
    RouteModule {
        merge_expr: "quickstart::routes::routes()",
        source: include_str!("quickstart/routes.rs"),
        function: "routes",
    },
];

const SERVER_MOD_SOURCE: &str = include_str!("mod.rs");

/// `source` without its `//` comments. A `//` inside a string literal (a URL,
/// say) is kept. Constructs this scanner cannot read reliably (block comments,
/// raw strings, a `'"'` character literal) panic rather than risk dropping or
/// keeping the wrong text.
fn strip_line_comments(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if in_string {
            out.push(ch);
            if ch == '\\' {
                out.extend(next);
                i += 1;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if ch == '/' && next == Some('*') {
            panic!("block comments are not supported by the route source scanner");
        } else if ch == '\'' && next == Some('"') && chars.get(i + 2) == Some(&'\'') {
            panic!("a '\"' character literal is not supported by the route source scanner");
        } else if ch == 'r'
            && matches!(next, Some('"' | '#'))
            && !chars
                .get(i.wrapping_sub(1))
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
        {
            panic!("raw strings are not supported by the route source scanner");
        } else {
            out.push(ch);
            in_string = ch == '"';
        }
        i += 1;
    }
    out
}

/// The text between the braces of `fn <name>(..) .. { .. }`.
fn function_body(source: &str, name: &str) -> String {
    let source = strip_line_comments(source);
    let signature = format!("fn {name}(");
    let start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("no `{signature}` in the route source"));
    let open = start + source[start..].find('{').expect("function body");
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open + 1..open + offset].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in `{name}`");
}

/// The text inside the parentheses that open at `open` (which holds a `(`).
fn balanced_parens(text: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (offset, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses in route source");
}

/// The `(METHOD, path)` pairs a router-building function registers, read from
/// its `.route("<path>", <method>(..).<method>(..))` calls.
///
/// Any other way of adding routes or reshaping the router (`nest`, `merge`,
/// `route_service`, `any`, `on`, ...) is refused: the test would otherwise
/// pass without covering what that call registers.
fn registered_routes(body: &str) -> BTreeSet<(String, String)> {
    let unsupported = Regex::new(
        r"\.(route_service|route_layer|nest|nest_service|merge|fallback|fallback_service|layer|with_state)\s*\(|\b(any|on|head|options|trace|connect|get_service|post_service)\s*\(",
    )
    .unwrap();
    assert!(
        !unsupported.is_match(body),
        "route function uses a registration style the guard test cannot enumerate: {body}"
    );
    let methods = Regex::new(r"\b(get|post|put|patch|delete)\s*\(").unwrap();
    let path = Regex::new(r#"^\s*"([^"]+)"\s*,"#).unwrap();

    let mut routes = BTreeSet::new();
    for (at, _) in body.match_indices(".route(") {
        let args = balanced_parens(body, at + ".route".len());
        let captures = path
            .captures(args)
            .unwrap_or_else(|| panic!("`.route(` without a literal path: {args}"));
        let route_path = captures[1].to_string();
        let handlers = &args[captures.get(0).unwrap().end()..];
        let found: Vec<String> = methods
            .captures_iter(handlers)
            .map(|m| m[1].to_uppercase())
            .collect();
        assert!(!found.is_empty(), "no method for route {route_path}");
        for method in found {
            assert!(
                routes.insert((method.clone(), route_path.clone())),
                "{method} {route_path} is registered twice"
            );
        }
    }
    routes
}

fn registered_platform_routes() -> BTreeSet<(String, String)> {
    ROUTE_MODULES
        .iter()
        .flat_map(|module| {
            let body = function_body(module.source, module.function);
            registered_routes(&body)
        })
        .collect()
}

#[test]
fn route_parser_understands_the_registration_styles_in_use() {
    let body = r#"
        Router::new()
            // a comment mentioning .route("/ignored", get(x))
            .route("/a/{id}", post(h::create).get(h::list))
            .route(
                "/a/{id}/b",
                get(super::h::one).patch(h::two).delete(h::three),
            )
    "#;
    let parsed: Vec<_> = registered_routes(&strip_line_comments(body))
        .into_iter()
        .collect();
    let expected: Vec<(String, String)> = [
        ("DELETE", "/a/{id}/b"),
        ("GET", "/a/{id}"),
        ("GET", "/a/{id}/b"),
        ("PATCH", "/a/{id}/b"),
        ("POST", "/a/{id}"),
    ]
    .into_iter()
    .map(|(m, p)| (m.to_string(), p.to_string()))
    .collect();
    assert_eq!(parsed, expected);
}

#[test]
#[should_panic(expected = "registration style")]
fn route_parser_refuses_registrations_it_cannot_enumerate() {
    registered_routes(r#"Router::new().nest("/x", inner()).route("/y", get(h))"#);
}

/// Refuse any call that adds routes or reshapes the router.
fn assert_no_route_registration(what: &str, text: &str) {
    let registering = Regex::new(
        r"\.(route|route_service|nest|nest_service|merge|fallback|fallback_service|with_state)\s*\(",
    )
    .unwrap();
    assert!(
        !registering.is_match(text),
        "{what} registers routes of its own, which the platform route guard test does not \
         cover; put them in a route module merged by `platform_route_tree`: {text}"
    );
}

#[test]
fn route_source_scanner_keeps_urls_in_strings_and_refuses_what_it_cannot_read() {
    let stripped = strip_line_comments(
        "let a = \"https://example.com\"; // trailing\n// whole line\n.route(\"/x\", get(h))",
    );
    assert!(stripped.contains("\"https://example.com\";"), "{stripped}");
    assert!(!stripped.contains("trailing") && !stripped.contains("whole line"));
    assert!(stripped.contains(".route(\"/x\", get(h))"));

    for unreadable in ["/* block */", "let r = r#\"raw\"#;", "let q = '\"';"] {
        let result = std::panic::catch_unwind(|| strip_line_comments(unreadable));
        assert!(result.is_err(), "{unreadable} was accepted");
    }
}

#[test]
fn route_modules_match_platform_route_tree() {
    let body = function_body(SERVER_MOD_SOURCE, "platform_route_tree");
    let mut remainder = body.clone();
    let mut merged = BTreeSet::new();
    let mut merge_calls = 0;
    while let Some(at) = remainder.find(".merge(") {
        let open = at + ".merge".len();
        let expr = balanced_parens(&remainder, open).to_string();
        merged.insert(expr.split_whitespace().collect::<String>());
        merge_calls += 1;
        let end = open + expr.len() + 2;
        remainder.replace_range(at..end, "");
    }
    // Everything but the `.merge(..)` calls must be the empty router they
    // start from: a `.route(..)` (or anything else) written in the tree itself
    // would otherwise be invisible to the route parser.
    assert_eq!(
        remainder.split_whitespace().collect::<String>(),
        "Router::new()",
        "platform_route_tree must consist of `Router::new()` and `.merge(..)` calls only; \
         register routes in a route module and merge it"
    );
    let known: BTreeSet<String> = ROUTE_MODULES
        .iter()
        .map(|module| module.merge_expr.to_string())
        .collect();
    assert_eq!(
        merged, known,
        "platform_route_tree merges different route modules than ROUTE_MODULES lists; \
         add the new module to ROUTE_MODULES and its routes to `cases()`"
    );
    assert_eq!(merge_calls, known.len(), "a route module is merged twice");
}

/// The tree reaches the server through `platform_routes` only, and nothing
/// adds routes to it on the way.
#[test]
fn platform_route_tree_reaches_the_server_unchanged() {
    let source = strip_line_comments(SERVER_MOD_SOURCE);
    // The definition and the one use inside `layered_platform_routes`.
    assert_eq!(
        source.matches("platform_route_tree()").count(),
        2,
        "platform_route_tree() must be used only by layered_platform_routes"
    );
    for name in ["layered_platform_routes", "platform_routes"] {
        let body = function_body(SERVER_MOD_SOURCE, name);
        assert_no_route_registration(&format!("`{name}`"), &body);
    }
    assert!(
        function_body(SERVER_MOD_SOURCE, "layered_platform_routes")
            .contains("platform_route_tree()"),
        "layered_platform_routes must build on platform_route_tree()"
    );

    // `run_server` takes the layered routes as they are.
    let run = function_body(SERVER_MOD_SOURCE, "run_server");
    let call = run
        .find("platform_routes(&state)")
        .expect("run_server must use platform_routes(&state)");
    assert_eq!(run.matches("platform_routes(").count(), 1);
    let statement = &run[call..call + run[call..].find(';').expect("statement end")];
    assert_eq!(
        statement.trim(),
        "platform_routes(&state)",
        "run_server must not chain anything onto platform_routes(&state)"
    );
    assert!(
        !Regex::new(r"\bplatform_routes\s*\.")
            .unwrap()
            .is_match(&run),
        "run_server must not chain anything onto the platform routes"
    );
}

// ---------------------------------------------------------------------------
// Fixture data
// ---------------------------------------------------------------------------

struct Seed {
    sa_id: Uuid,
    /// Claims every service account of `demo` requires.
    accepted_issuer: &'static str,
}

const ACCEPTED_ISSUER: &str = "https://ci.example.com";
/// A valid user code (the alphabet has no vowels) of the seeded device login.
const DEVICE_USER_CODE: &str = "BCDF-GHJK";
const OTHER_PROJECT_ISSUER: &str = "https://other-ci.example.com";

/// Seed a target project (`demo`, created by the fixture) that has a service
/// account for `ACCEPTED_ISSUER` plus one row for every kind of thing the
/// platform routes touch, and a second project whose service account trusts a
/// different issuer.
async fn seed(pool: &PgPool) -> Seed {
    let demo = db::projects::find_by_name(pool, "demo")
        .await
        .unwrap()
        .expect("fixture project");
    let production = db::environments::test_environment_id(pool, demo.id).await;

    let claims = [("sub".to_string(), "repo:acme/app".to_string())]
        .into_iter()
        .collect();
    let sa = db::service_accounts::create(pool, demo.id, ACCEPTED_ISSUER, &claims)
        .await
        .unwrap();

    let owner = db::users::create(pool, "other-owner@example.com")
        .await
        .unwrap();
    let other = db::projects::create(
        pool,
        "other-project",
        db::models::ProjectStatus::Running,
        "public".into(),
        Some(owner.id),
        None,
        None,
    )
    .await
    .unwrap();
    let other_claims = [("sub".to_string(), "repo:other/app".to_string())]
        .into_iter()
        .collect();
    db::service_accounts::create(pool, other.id, OTHER_PROJECT_ISSUER, &other_claims)
        .await
        .unwrap();

    db::custom_domains::add_custom_domain(pool, demo.id, production, "app.example.org")
        .await
        .unwrap();
    db::env_vars::upsert_project_env_var(pool, demo.id, "FOO", "bar", false, false, None)
        .await
        .unwrap();
    db::extensions::create(
        pool,
        demo.id,
        "ext1",
        "aws-rds-provisioner",
        &json!({"engine": "postgres"}),
    )
    .await
    .unwrap();
    db::teams::create(pool, "guard-team").await.unwrap();
    // A pending device login, so the `/auth/device*` requests name a code that
    // exists and would be found, approved or denied if the handler acted.
    let created = db::device_authorizations::create(
        pool,
        &db::device_authorizations::NewDeviceAuthorization {
            device_code_hash: b"guard-test-device-code-hash",
            user_code: DEVICE_USER_CODE,
            client_name: Some("guard test"),
            client_ip: None,
            requested_access: None,
            interval_seconds: 5,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        },
    )
    .await
    .unwrap();
    assert!(created);

    Seed {
        sa_id: sa.id,
        accepted_issuer: ACCEPTED_ISSUER,
    }
}

/// A hash of every row of every table, so any write shows up as a difference.
async fn database_snapshot(pool: &PgPool) -> BTreeMap<String, String> {
    let digests: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_schema::text || '.' || table_name::text, \
                md5(query_to_xml( \
                    format('SELECT t::text AS r FROM %I.%I t ORDER BY 1', \
                           table_schema, table_name), \
                    false, true, '')::text) \
         FROM information_schema.tables \
         WHERE table_type = 'BASE TABLE' \
           AND table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(digests.len() > 10, "snapshot found no tables: {digests:?}");
    digests.into_iter().collect()
}

// ---------------------------------------------------------------------------
// The route table
// ---------------------------------------------------------------------------

struct Case {
    method: Method,
    /// The path as registered, e.g. `/projects/{id_or_name}/env/{key}`.
    pattern: &'static str,
    /// A request URI that matches `pattern` and names seeded data.
    uri: String,
    /// A body the handler's extractor accepts; harmful if the handler acted.
    body: Option<Value>,
}

/// One request per registered `(method, path)`, with plausible parameters and a
/// valid body so extraction succeeds and the handler itself decides.
fn cases(seed: &Seed) -> Vec<Case> {
    let sa = seed.sa_id;
    let case = |method: Method, pattern: &'static str, uri: &str, body: Option<Value>| Case {
        method,
        pattern,
        uri: uri.to_string(),
        body,
    };
    let get = |pattern: &'static str, uri: &str| case(Method::GET, pattern, uri, None);
    let delete = |pattern: &'static str, uri: &str| case(Method::DELETE, pattern, uri, None);
    let post = |pattern: &'static str, uri: &str, body: Value| {
        case(Method::POST, pattern, uri, Some(body))
    };
    let put =
        |pattern: &'static str, uri: &str, body: Value| case(Method::PUT, pattern, uri, Some(body));
    let patch = |pattern: &'static str, uri: &str, body: Value| {
        case(Method::PATCH, pattern, uri, Some(body))
    };

    let device_decision = json!({"user_code": DEVICE_USER_CODE, "access": {"kind": "full"}});
    let deployment_status = json!({"status": "Failed", "error_message": "tampered"});
    let oauth_spec = json!({
        "provider_name": "Attacker IdP",
        "client_id": "client",
        "issuer_url": "https://idp.example.com",
    });

    vec![
        // auth::routes::platform_routes
        post(
            "/users/lookup",
            "/users/lookup",
            json!({"emails": ["owner@example.com"]}),
        ),
        get(
            "/auth/device",
            &format!("/auth/device?user_code={DEVICE_USER_CODE}"),
        ),
        post(
            "/auth/device/approve",
            "/auth/device/approve",
            device_decision.clone(),
        ),
        post("/auth/device/deny", "/auth/device/deny", device_decision),
        // custom_domains
        post(
            "/projects/{project_id_or_name}/domains",
            "/projects/demo/domains",
            json!({"domain": "evil.example.org"}),
        ),
        get(
            "/projects/{project_id_or_name}/domains",
            "/projects/demo/domains",
        ),
        get(
            "/projects/{project_id_or_name}/domains/{domain}",
            "/projects/demo/domains/app.example.org",
        ),
        patch(
            "/projects/{project_id_or_name}/domains/{domain}",
            "/projects/demo/domains/app.example.org",
            json!({"environment": "production"}),
        ),
        delete(
            "/projects/{project_id_or_name}/domains/{domain}",
            "/projects/demo/domains/app.example.org",
        ),
        put(
            "/projects/{project_id_or_name}/domains/{domain}/primary",
            "/projects/demo/domains/app.example.org/primary",
            json!({}),
        ),
        delete(
            "/projects/{project_id_or_name}/domains/{domain}/primary",
            "/projects/demo/domains/app.example.org/primary",
        ),
        // project
        get("/projects/access-classes", "/projects/access-classes"),
        get("/projects", "/projects"),
        get("/teams/{id_or_name}/projects", "/teams/guard-team/projects"),
        post(
            "/projects",
            "/projects",
            json!({
                "name": "squatted",
                "access_class": "public",
                "owner": {"team": "guard-team"},
            }),
        ),
        get("/projects/{id_or_name}", "/projects/demo"),
        put(
            "/projects/{id_or_name}",
            "/projects/demo",
            json!({"access_class": "private", "source_url": "https://evil.example.com"}),
        ),
        delete("/projects/{id_or_name}", "/projects/demo"),
        patch(
            "/projects/{id_or_name}/template-image",
            "/projects/demo/template-image",
            json!({"image": "evil:latest"}),
        ),
        // team
        post(
            "/teams",
            "/teams",
            json!({"name": "evil-team", "members": [], "owners": []}),
        ),
        get("/teams", "/teams"),
        get("/teams/{id}", "/teams/guard-team"),
        put("/teams/{id}", "/teams/guard-team", json!({"name": "evil"})),
        delete("/teams/{id}", "/teams/guard-team"),
        // registry
        get(
            "/projects/{project_name}/deployments/{deployment_id}/registry-credentials",
            "/projects/demo/deployments/20261027-100000/registry-credentials",
        ),
        // deployment
        post(
            "/deployments",
            "/deployments",
            json!({"project": "demo", "image": "evil:latest"}),
        ),
        patch(
            "/deployments/{deployment_id}/status",
            "/deployments/20261027-100000/status",
            deployment_status.clone(),
        ),
        get(
            "/projects/{project_name}/deployments",
            "/projects/demo/deployments",
        ),
        get(
            "/projects/{project_name}/deployment-groups",
            "/projects/demo/deployment-groups",
        ),
        post(
            "/projects/{project_name}/deployments/stop",
            "/projects/demo/deployments/stop?group=default",
            json!({}),
        ),
        get(
            "/projects/{project_name}/deployments/{deployment_id}",
            "/projects/demo/deployments/20261027-100000",
        ),
        patch(
            "/projects/{project_name}/deployments/{deployment_id}/status",
            "/projects/demo/deployments/20261027-100000/status",
            deployment_status,
        ),
        post(
            "/projects/{project_name}/deployments/{deployment_id}/stop",
            "/projects/demo/deployments/20261027-100000/stop",
            json!({}),
        ),
        get(
            "/projects/{project_name}/deployments/{deployment_id}/containers",
            "/projects/demo/deployments/20261027-100000/containers",
        ),
        get(
            "/projects/{project_name}/deployments/{deployment_id}/events",
            "/projects/demo/deployments/20261027-100000/events",
        ),
        get(
            "/projects/{project_name}/deployments/{deployment_id}/logs",
            "/projects/demo/deployments/20261027-100000/logs",
        ),
        get(
            "/projects/{project_name}/deployments/{deployment_id}/logs/volume",
            "/projects/demo/deployments/20261027-100000/logs/volume\
             ?start=2026-01-01T00:00:00Z&end=2026-01-02T00:00:00Z",
        ),
        // service_accounts
        post(
            "/projects/{project_name}/service-accounts",
            "/projects/demo/service-accounts",
            json!({"issuer_url": "https://evil.example.com", "claims": {"sub": "attacker"}}),
        ),
        get(
            "/projects/{project_name}/service-accounts",
            "/projects/demo/service-accounts",
        ),
        get(
            "/projects/{project_name}/service-accounts/{sa_id}",
            &format!("/projects/demo/service-accounts/{sa}"),
        ),
        put(
            "/projects/{project_name}/service-accounts/{sa_id}",
            &format!("/projects/demo/service-accounts/{sa}"),
            json!({"issuer_url": "https://evil.example.com", "claims": {"sub": "attacker"}}),
        ),
        delete(
            "/projects/{project_name}/service-accounts/{sa_id}",
            &format!("/projects/demo/service-accounts/{sa}"),
        ),
        // env_vars
        put(
            "/projects/{project_id_or_name}/env/{key}",
            "/projects/demo/env/FOO",
            json!({"value": "pwned", "is_secret": false}),
        ),
        delete(
            "/projects/{project_id_or_name}/env/{key}",
            "/projects/demo/env/FOO",
        ),
        post(
            "/projects/{project_id_or_name}/env/{key}/move",
            "/projects/demo/env/FOO/move",
            json!({"from_environment": null, "to_environment": "production"}),
        ),
        get(
            "/projects/{project_id_or_name}/env/{key}/value",
            "/projects/demo/env/FOO/value",
        ),
        get("/projects/{project_id_or_name}/env", "/projects/demo/env"),
        get(
            "/projects/{project_id_or_name}/env/preview",
            "/projects/demo/env/preview",
        ),
        get(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env",
            "/projects/demo/deployments/20261027-100000/env",
        ),
        get(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env/diff",
            "/projects/demo/deployments/20261027-100000/env/diff",
        ),
        get(
            "/projects/{project_id_or_name}/deployments/{deployment_id}/env/{key}/value",
            "/projects/demo/deployments/20261027-100000/env/FOO/value",
        ),
        // environments
        get(
            "/projects/{project}/environments",
            "/projects/demo/environments",
        ),
        post(
            "/projects/{project}/environments",
            "/projects/demo/environments",
            json!({"name": "evil-env"}),
        ),
        get(
            "/projects/{project}/environments/{name}",
            "/projects/demo/environments/production",
        ),
        patch(
            "/projects/{project}/environments/{name}",
            "/projects/demo/environments/production",
            json!({"name": "hijacked", "is_production": false}),
        ),
        delete(
            "/projects/{project}/environments/{name}",
            "/projects/demo/environments/production",
        ),
        // extensions
        get("/extensions/types", "/extensions/types"),
        post(
            "/projects/{project}/extensions/{extension}/oauth/credential-binding",
            "/projects/demo/extensions/ext1/oauth/credential-binding",
            oauth_spec,
        ),
        get(
            "/projects/{project}/extensions",
            "/projects/demo/extensions",
        ),
        post(
            "/projects/{project}/extensions/{extension}",
            "/projects/demo/extensions/ext2",
            json!({"extension_type": "aws-rds-provisioner", "spec": {"engine": "postgres"}}),
        ),
        put(
            "/projects/{project}/extensions/{extension}",
            "/projects/demo/extensions/ext1",
            json!({"spec": {"engine": "mysql"}}),
        ),
        patch(
            "/projects/{project}/extensions/{extension}",
            "/projects/demo/extensions/ext1",
            json!({"spec": {"engine": "mysql"}}),
        ),
        get(
            "/projects/{project}/extensions/{extension}",
            "/projects/demo/extensions/ext1",
        ),
        delete(
            "/projects/{project}/extensions/{extension}",
            "/projects/demo/extensions/ext1",
        ),
        // encryption
        post(
            "/encrypt",
            "/encrypt",
            json!({
                "plaintext": "secret",
                "project": "demo",
                "extension_type": "aws-rds-provisioner",
                "extension": "ext1",
                "slot": "/password",
            }),
        ),
        // quickstart
        get("/quickstart-templates", "/quickstart-templates"),
    ]
}

#[sqlx::test]
async fn route_table_covers_every_registered_platform_route(pool: PgPool) {
    let fixture = fixture(pool.clone()).await;
    let _ = &fixture;
    let seed = seed(&pool).await;

    let table: BTreeSet<(String, String)> = cases(&seed)
        .into_iter()
        .map(|case| (case.method.to_string(), case.pattern.to_string()))
        .collect();
    let registered = registered_platform_routes();

    let uncovered: Vec<_> = registered.difference(&table).collect();
    let stale: Vec<_> = table.difference(&registered).collect();
    assert!(
        uncovered.is_empty() && stale.is_empty(),
        "the platform route table in platform_route_guard_tests.rs has drifted from the router.\n\
         Registered but not exercised (add a case in `cases()`): {uncovered:#?}\n\
         Exercised but not registered (remove or fix the case): {stale:#?}"
    );
    assert_eq!(
        table.len(),
        cases(&seed).len(),
        "two cases target the same route"
    );
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The platform route tree with every layer `run` puts on it, except that a
/// layer stamping `token` into the request replaces `auth_middleware`. The
/// fallback marks requests that matched no route.
fn platform_router(state: &AppState, token: Option<VerifiedExternalToken>) -> Router {
    let stamp = move |mut req: Request, next: Next| {
        let token = token.clone();
        async move {
            if let Some(token) = token {
                req.extensions_mut().insert(token);
            }
            next.run(req).await
        }
    };
    let tree = crate::server::layered_platform_routes(state, |routes| {
        routes.route_layer(middleware::from_fn(stamp))
    });
    Router::new()
        .nest("/api/v1", tree)
        .fallback(|| async { NOT_ROUTED.into_response() })
        .with_state(state.clone())
}

async fn send(router: &Router, case: &Case) -> Response {
    let mut request = Request::builder()
        .method(case.method.clone())
        .uri(format!("/api/v1{}", case.uri));
    let body = match &case.body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

fn token(issuer: &str, sub: &str) -> VerifiedExternalToken {
    VerifiedExternalToken {
        issuer: issuer.to_string(),
        claims: json!({"iss": issuer, "sub": sub, "aud": "rise"}),
    }
}

#[sqlx::test]
async fn external_token_matching_no_service_account_is_refused_by_every_platform_route(
    pool: PgPool,
) {
    let fixture = fixture(pool.clone()).await;
    let state = &fixture.state;
    let seed = seed(&pool).await;
    let cases = cases(&seed);

    // Both ways a token can fail to match `demo`'s service accounts: its
    // issuer is trusted there but the claims differ (a stranger minting a token
    // at a public CI issuer), and its issuer is trusted only by another
    // project's service account.
    let strangers = [
        (
            "right issuer, wrong claims",
            token(seed.accepted_issuer, "repo:stranger/app"),
        ),
        (
            "issuer trusted by another project only",
            token(OTHER_PROJECT_ISSUER, "repo:other/app"),
        ),
    ];

    // Control pass. The same requests from a token that does match `demo`'s
    // service account, against the same seeded data. It exists for the
    // refusals a stranger can only tell from success by status: several
    // handlers answer 404 where they mean "not yours" (and a handler with its
    // check removed would answer 404 for an unknown code or row just as well).
    // A 403/404 for a stranger counts only if the matching service account got
    // past it on that very request, so the seeded data really does make the
    // request succeed for an authorized caller and the 404 is the auth check.
    // Run before the snapshot because the requests that succeed change state.
    assert!(
        db::device_authorizations::find_pending_by_user_code(&pool, DEVICE_USER_CODE)
            .await
            .unwrap()
            .is_some(),
        "the seeded device login must be pending"
    );
    let matching = platform_router(state, Some(token(seed.accepted_issuer, "repo:acme/app")));
    let mut controls = Vec::new();
    for case in &cases {
        controls.push(send(&matching, case).await.status());
    }
    let accepted = |status: StatusCode| {
        !matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        )
    };
    assert!(
        cases
            .iter()
            .zip(&controls)
            .any(
                |(case, status)| case.pattern == "/projects/{project_name}/deployments"
                    && *status == StatusCode::OK
            ),
        "control: a token matching the project's service account must be accepted: {controls:?}"
    );

    let before = database_snapshot(&pool).await;
    let runtime_actions_before = fixture.backend.runtime_actions.load(Ordering::SeqCst);
    let mut failures = Vec::new();
    let mut masked = 0;
    for (label, stranger) in strangers {
        let router = platform_router(state, Some(stranger));
        for (case, control) in cases.iter().zip(&controls) {
            let response = send(&router, case).await;
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&body);
            let explicit_refusal = status == StatusCode::UNAUTHORIZED
                && [
                    // `AuthContext::user`
                    "does not support service account authentication",
                    // `AuthContext::resolve_for_project`
                    "No service accounts configured for issuer",
                    "Token claims do not match any service account",
                ]
                .iter()
                .any(|message| body.contains(message));
            let masked_refusal = matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
                && accepted(*control);
            masked += usize::from(masked_refusal);
            if !(explicit_refusal || masked_refusal) {
                failures.push(format!(
                    "[{label}] {} {} -> {status} {body} (matching service account: {control})",
                    case.method, case.uri
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "an external token no service account of the project accepts was not refused. A refusal \
         is a 401 saying the endpoint takes no service account or that no service account \
         matches, or a 403/404 on a request the matching service account got past. A 2xx means \
         the handler acted on the token; 400/415/422 means the request never reached the \
         handler and the case needs a valid body; 418 means the route is not registered:\n{}",
        failures.join("\n")
    );
    assert!(masked > 0, "no request relied on a masked 404/403");

    assert_eq!(
        before,
        database_snapshot(&pool).await,
        "a refused external-token request changed the database"
    );
    assert_eq!(
        runtime_actions_before,
        fixture.backend.runtime_actions.load(Ordering::SeqCst),
        "a refused external-token request reached the deployment backend"
    );
}
