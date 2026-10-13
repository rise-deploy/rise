//! HTTP-provider routing snapshots.
//!
//! Docker and ECS keep native-provider services because those providers are
//! best placed to discover volatile server membership. Rise owns the stable
//! public routers and points them at one deployment-scoped native service.

use std::collections::BTreeMap;

use rise_backend_core::desired::{DesiredContainer, DesiredRoute};
use rise_backend_core::models::{CustomDomain, Deployment, Environment, Project};
use rise_backend_core::{resolve_runtime_containers, DeploymentUrlBuilder};
use rise_deployment_spec::AccessRequirement;
use serde::{Deserialize, Serialize};

use crate::labels::build_rule;
use crate::naming::{deployment_service_base, group_service_base, group_service_name};
use crate::render::{routes_withheld, TraefikRenderConfig};

#[derive(Debug, Clone)]
pub struct DynamicRouteTarget {
    pub project: String,
    pub access_class: String,
    pub deployment_group: String,
    pub deployment_id: String,
    pub container: String,
    pub port: Option<u16>,
    pub routes: Vec<DesiredRoute>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DynamicConfig {
    #[serde(default)]
    pub http: DynamicHttp,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DynamicHttp {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routers: BTreeMap<String, DynamicRouter>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub middlewares: BTreeMap<String, DynamicMiddleware>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicRouter {
    pub rule: String,
    pub entry_points: Vec<String>,
    pub service: String,
    pub priority: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub middlewares: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<DynamicTls>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicTls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_resolver: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicMiddleware {
    pub forward_auth: DynamicForwardAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicForwardAuth {
    pub address: String,
    pub auth_response_headers: Vec<String>,
}

/// Render every public route for one selected deployment.
pub fn render_dynamic_config(
    containers: &[DesiredContainer],
    cfg: &TraefikRenderConfig<'_>,
    native_provider: &str,
) -> DynamicConfig {
    render_dynamic_route_targets(
        containers.iter().map(|desired| DynamicRouteTarget {
            project: desired.project.clone(),
            access_class: desired.access_class.clone(),
            deployment_group: desired.deployment_group.clone(),
            deployment_id: desired.deployment_id.clone(),
            container: desired.container.clone(),
            port: desired.port.filter(|_| desired.routable),
            routes: desired.routes.clone(),
        }),
        cfg,
        native_provider,
    )
}

/// Route targets for `project`'s active deployments — one per deployment
/// group, each routed on its own group's hosts
/// ([`DeploymentUrlBuilder::public_hosts_for_group`]).
///
/// A deployment whose container/route side-data does not decode fails the
/// whole call: the HTTP-provider endpoint then errors and Traefik keeps
/// serving its last good config, rather than dropping a live router.
pub fn project_route_targets(
    project: &Project,
    active_deployments: &[Deployment],
    all_environments: &[Environment],
    custom_domains: &[CustomDomain],
    url_builder: &DeploymentUrlBuilder,
) -> anyhow::Result<Vec<DynamicRouteTarget>> {
    let mut targets = Vec::new();
    for deployment in active_deployments {
        let (containers, routes) = resolve_runtime_containers(deployment)?;
        let hosts = url_builder.public_hosts_for_group(
            project,
            &deployment.deployment_group,
            all_environments,
            custom_domains,
        );
        targets.extend(containers.into_iter().map(|container| {
            DynamicRouteTarget {
                project: project.name.clone(),
                access_class: project.access_class.clone(),
                deployment_group: deployment.deployment_group.clone(),
                deployment_id: deployment.deployment_id.clone(),
                port: container.port,
                routes: routes
                    .iter()
                    .filter(|route| route.container == container.name)
                    .map(|route| DesiredRoute {
                        hosts: hosts.clone(),
                        path_prefix: Some(route.path.clone()),
                        access: route.access.clone(),
                    })
                    .collect(),
                container: container.name,
            }
        }));
    }
    Ok(targets)
}

pub fn render_dynamic_route_targets(
    targets: impl IntoIterator<Item = DynamicRouteTarget>,
    cfg: &TraefikRenderConfig<'_>,
    native_provider: &str,
) -> DynamicConfig {
    let mut out = DynamicConfig::default();
    for desired in targets {
        if desired.port.is_none() {
            continue;
        }
        if routes_withheld(
            &desired.access_class,
            cfg.access_classes,
            cfg.auth_backend_url,
            desired.routes.iter().map(|route| route.access.as_ref()),
        ) {
            continue;
        }
        let Some(project_requirement) = cfg.access_classes.get(&desired.access_class) else {
            continue;
        };

        let mut routes = desired.routes.clone();
        routes.sort_by(|a, b| {
            b.path_prefix
                .as_deref()
                .unwrap_or("/")
                .len()
                .cmp(&a.path_prefix.as_deref().unwrap_or("/").len())
        });
        let route_count = routes.len();
        let router_base = group_service_base(
            &desired.project,
            &desired.deployment_group,
            &desired.container,
        );
        let service_base = deployment_service_base(
            &desired.project,
            &desired.deployment_group,
            &desired.deployment_id,
            &desired.container,
        );

        for (idx, route) in routes.iter().enumerate() {
            if route.hosts.is_empty() {
                continue;
            }
            let router_name = group_service_name(&router_base, idx, route_count);
            let service_name = group_service_name(&service_base, idx, route_count);
            let effective = route.access.as_ref().unwrap_or(project_requirement);
            let middleware_name = format!("{router_name}-auth");
            let middlewares = match effective {
                AccessRequirement::None => Vec::new(),
                AccessRequirement::Authenticated | AccessRequirement::Member => {
                    let address = format!(
                        "{}/api/v1/auth/ingress?project={}&access={}&signin_redirect=1",
                        cfg.auth_backend_url.trim().trim_end_matches('/'),
                        urlencoding::encode(&desired.project),
                        effective.as_query_param(),
                    );
                    out.http.middlewares.insert(
                        middleware_name.clone(),
                        DynamicMiddleware {
                            forward_auth: DynamicForwardAuth {
                                address,
                                auth_response_headers: vec![
                                    "X-Auth-Request-Email".to_string(),
                                    "X-Auth-Request-User".to_string(),
                                ],
                            },
                        },
                    );
                    vec![middleware_name]
                }
            };
            let path_len = route
                .path_prefix
                .as_deref()
                .filter(|path| !path.is_empty() && *path != "/")
                .map(str::len)
                .unwrap_or(0);
            out.http.routers.insert(
                router_name,
                DynamicRouter {
                    rule: build_rule(&route.hosts, route.path_prefix.as_deref()),
                    entry_points: vec![cfg.traefik_entrypoint.to_string()],
                    service: format!("{service_name}@{native_provider}"),
                    priority: path_len + 1,
                    middlewares,
                    tls: cfg.traefik_certresolver.map(|resolver| DynamicTls {
                        cert_resolver: Some(resolver.to_string()),
                    }),
                },
            );
        }
    }
    out
}

pub fn merge_dynamic_configs(configs: impl IntoIterator<Item = DynamicConfig>) -> DynamicConfig {
    let mut merged = DynamicConfig::default();
    for config in configs {
        merged.http.routers.extend(config.http.routers);
        merged.http.middlewares.extend(config.http.middlewares);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_backend_core::test_helpers::{
        custom_domain, deployment, desired, environment, project, url_builder,
    };
    use std::collections::HashMap;

    fn config<'a>(classes: &'a HashMap<String, AccessRequirement>) -> TraefikRenderConfig<'a> {
        TraefikRenderConfig {
            label_namespace: "rise.dev",
            controller_class: "default",
            traefik_entrypoint: "websecure",
            catalog_entrypoint: "rise-catalog",
            traefik_certresolver: Some("le"),
            network: None,
            auth_backend_url: "http://rise:3000",
            access_classes: classes,
        }
    }

    #[test]
    fn router_name_stays_stable_while_service_follows_deployment() {
        let classes = HashMap::from([("public".to_string(), AccessRequirement::None)]);
        let mut first = desired("app", "img:1", "h1");
        first.access_class = "public".to_string();
        let mut second = first.clone();
        second.deployment_id = "next-deployment".to_string();

        let first_config = render_dynamic_config(&[first], &config(&classes), "ecs");
        let second_config = render_dynamic_config(&[second], &config(&classes), "ecs");
        assert_eq!(
            first_config.http.routers.keys().collect::<Vec<_>>(),
            second_config.http.routers.keys().collect::<Vec<_>>()
        );
        assert_ne!(
            first_config.http.routers.values().next().unwrap().service,
            second_config.http.routers.values().next().unwrap().service
        );
        assert!(serde_json::to_value(first_config).unwrap()["http"]
            .get("middlewares")
            .is_none());
    }

    #[test]
    fn public_router_carries_path_auth_priority_and_tls() {
        let classes = HashMap::from([("private".to_string(), AccessRequirement::Member)]);
        let mut container = desired("app", "img:1", "h1");
        container.access_class = "private".to_string();
        container.routes = vec![DesiredRoute {
            hosts: vec!["myapp.example.com".to_string()],
            path_prefix: Some("/admin".to_string()),
            access: None,
        }];

        let rendered = render_dynamic_config(&[container], &config(&classes), "docker");
        let (name, router) = rendered.http.routers.iter().next().unwrap();
        assert_eq!(
            router.rule,
            "Host(`myapp.example.com`) && (Path(`/admin`) || PathPrefix(`/admin/`))"
        );
        assert_eq!(router.priority, 7);
        assert_eq!(router.entry_points, vec!["websecure"]);
        assert!(router.service.ends_with("@docker"));
        assert_eq!(router.middlewares, vec![format!("{name}-auth")]);
        assert_eq!(
            router.tls.as_ref().unwrap().cert_resolver.as_deref(),
            Some("le")
        );
        assert_eq!(
            rendered
                .http
                .middlewares
                .get(&format!("{name}-auth"))
                .unwrap()
                .forward_auth
                .address,
            "http://rise:3000/api/v1/auth/ingress?project=myapp&access=Member&signin_redirect=1"
        );
    }

    /// Regression for rise-deploy/rise#548: every active deployment group is
    /// routed on its own hosts with its own auth middleware, and a production
    /// custom domain stays on the production group's router.
    #[test]
    fn every_active_group_is_routed_on_its_own_hosts() {
        let classes = HashMap::from([("private".to_string(), AccessRequirement::Member)]);
        let project = project("docs-app", "private");
        let production = environment(&project, "production", Some("default"));
        let deployments = [
            deployment(&project, "d3", "pr/124"),
            deployment(&project, "d2", "pr/123"),
            deployment(&project, "d1", "default"),
        ];

        let rendered = render_dynamic_route_targets(
            project_route_targets(
                &project,
                &deployments,
                std::slice::from_ref(&production),
                &[custom_domain(&production, "docs.customer.com")],
                &url_builder(),
            )
            .unwrap(),
            &config(&classes),
            "ecs",
        );

        assert_eq!(rendered.http.routers.len(), 3, "{rendered:#?}");
        let rule_for = |host: &str| {
            rendered
                .http
                .routers
                .values()
                .map(|router| router.rule.as_str())
                .find(|rule| rule.contains(&format!("Host(`{host}`)")))
                .unwrap_or_else(|| panic!("no router for {host}"))
        };
        let production_rule = rule_for("docs-app-default.preview.example.test");
        assert!(production_rule.contains("Host(`docs-app.example.test`)"));
        assert!(production_rule.contains("Host(`docs.customer.com`)"));
        for preview in [
            "docs-app-pr--123.preview.example.test",
            "docs-app-pr--124.preview.example.test",
        ] {
            assert!(!rule_for(preview).contains("docs.customer.com"));
        }
        for (name, router) in &rendered.http.routers {
            assert_eq!(router.middlewares, vec![format!("{name}-auth")]);
            assert!(rendered
                .http
                .middlewares
                .contains_key(&format!("{name}-auth")));
        }
    }

    /// An undecodable active deployment fails the snapshot (Traefik keeps its
    /// last good config) instead of silently dropping that group's router.
    #[test]
    fn undecodable_deployment_fails_the_snapshot() {
        let project = project("docs-app", "private");
        let mut corrupt = deployment(&project, "d2", "pr/123");
        corrupt.containers = Some(serde_json::json!("not a container list"));
        let deployments = [deployment(&project, "d1", "default"), corrupt];
        assert!(project_route_targets(&project, &deployments, &[], &[], &url_builder()).is_err());
    }
}
