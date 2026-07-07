use crate::request_spec::{ContainerSpec, RouteSpec};
use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Validate `^[a-z][a-z0-9-]{0,14}$` (max 15 chars, starts with lowercase
/// letter, lowercase alphanumeric + dash, no trailing dash).
pub fn is_valid_container_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 15 {
        return false;
    }
    if name.ends_with('-') {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty by len() check above");
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn validate_route_path(path: &str) -> Result<(), ValidationError> {
    if !path.starts_with('/') {
        return Err(ValidationError::new(format!(
            "Route path '{}' must start with '/'",
            path
        )));
    }
    if path == "/.rise" || path.starts_with("/.rise/") {
        return Err(ValidationError::new(format!(
            "Route path '{}' uses the reserved '/.rise' platform prefix",
            path
        )));
    }
    if !path[1..]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
    {
        return Err(ValidationError::new(format!(
            "Route path '{}' contains invalid characters; allowed: letters, digits, and '. _ ~ / -'",
            path
        )));
    }
    Ok(())
}

pub fn validate_containers_and_routes(
    containers: Option<&[ContainerSpec]>,
    routes: &[RouteSpec],
) -> Result<(), ValidationError> {
    let Some(containers) = containers else {
        if !routes.is_empty() {
            return Err(ValidationError::new(
                "routes requires at least one container",
            ));
        }
        return Ok(());
    };

    if containers.is_empty() {
        return Err(ValidationError::new(
            "containers list must not be empty when present",
        ));
    }

    let mut seen_names = HashSet::with_capacity(containers.len());
    for spec in containers {
        if !is_valid_container_name(&spec.name) {
            return Err(ValidationError::new(format!(
                "Invalid container name '{}': must match ^[a-z][a-z0-9-]{{0,14}}$ (max 15 chars, no trailing dash)",
                spec.name
            )));
        }
        if !seen_names.insert(spec.name.clone()) {
            return Err(ValidationError::new(format!(
                "Duplicate container name '{}'",
                spec.name
            )));
        }
        if let Some(ref img) = spec.image {
            if img.is_empty() {
                return Err(ValidationError::new(format!(
                    "Container '{}' has an empty image; either omit it (build from source) or set a non-empty value",
                    spec.name
                )));
            }
        }
        if spec.health_check.is_some() && spec.port.is_none() {
            return Err(ValidationError::new(format!(
                "Container '{}' has health_check but no port; set port or remove health_check",
                spec.name
            )));
        }
        for over in &spec.env_overrides {
            if over.is_secret {
                return Err(ValidationError::new(format!(
                    "Per-container secret env overrides are not yet supported (container '{}', key '{}'). Use a project-level secret env var, or include the secret in the per-container plain env after appropriate handling.",
                    spec.name, over.key
                )));
            }
        }
    }

    let container_by_name: HashMap<&str, &ContainerSpec> =
        containers.iter().map(|c| (c.name.as_str(), c)).collect();

    for route in routes {
        validate_route_path(&route.path)?;
        let target = container_by_name
            .get(route.container.as_str())
            .ok_or_else(|| {
                ValidationError::new(format!(
                    "Route '{}' targets unknown container '{}'",
                    route.path, route.container
                ))
            })?;
        if target.port.is_none() {
            return Err(ValidationError::new(format!(
                "Route '{}' targets container '{}' which has no port",
                route.path, route.container
            )));
        }
    }

    Ok(())
}

pub fn validate_project_config(
    config: &crate::project_config::ProjectBuildConfig,
) -> Result<(), ValidationError> {
    if config.containers.is_empty() {
        if !config.routes.is_empty() {
            return Err(ValidationError::new(
                "[routes] requires at least one [containers.<name>] entry",
            ));
        }
        return Ok(());
    }

    let top_build_present = config.build.is_some();

    for (name, container) in &config.containers {
        if !is_valid_container_name(name) {
            return Err(ValidationError::new(format!(
                "Invalid container name '{}': must match ^[a-z][a-z0-9-]{{0,14}}$ (max 15 chars, no trailing dash)",
                name
            )));
        }
        match (&container.image, &container.build) {
            (Some(_), Some(_)) => {
                return Err(ValidationError::new(format!(
                    "[containers.{}] cannot set both 'image' and [build]; pick one",
                    name
                )));
            }
            (Some(img), None) if img.is_empty() => {
                return Err(ValidationError::new(format!(
                    "[containers.{}] image must not be empty",
                    name
                )));
            }
            (None, None) if !top_build_present => {
                return Err(ValidationError::new(format!(
                    "[containers.{}] must set either 'image' or [build] \
                 (or inherit a top-level [build])",
                    name
                )));
            }
            _ => {}
        }
        let container_health_check = container
            .deploy
            .as_ref()
            .and_then(|d| d.health_check.as_ref());
        if container_health_check.is_some() && container.port.is_none() {
            return Err(ValidationError::new(format!(
                "[containers.{}] has a [deploy].health_check but no port; \
                 set port or remove health_check",
                name
            )));
        }
    }

    for (path, route) in &config.routes {
        validate_project_route_path(path)?;
        let target = config.containers.get(&route.container).ok_or_else(|| {
            ValidationError::new(format!(
                "[routes] '{}' targets unknown container '{}'",
                path, route.container
            ))
        })?;
        if target.port.is_none() {
            return Err(ValidationError::new(format!(
                "[routes] '{}' targets container '{}' which has no port",
                path, route.container
            )));
        }
    }

    Ok(())
}

pub fn validate_project_route_path(path: &str) -> Result<(), ValidationError> {
    if !path.starts_with('/') {
        return Err(ValidationError::new(format!(
            "[routes] path '{}' must start with '/'",
            path
        )));
    }
    if path == "/.rise" || path.starts_with("/.rise/") {
        return Err(ValidationError::new(format!(
            "[routes] path '{}' uses the reserved '/.rise' platform prefix",
            path
        )));
    }
    if !path[1..]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
    {
        return Err(ValidationError::new(format!(
            "[routes] path '{}' contains invalid characters; allowed: letters, digits, and '. _ ~ / -'",
            path
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_spec::{ContainerSpec, EnvOverride, HealthCheckSpec, RouteSpec};

    fn cspec(name: &str, image: Option<&str>, port: Option<u16>) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            image: image.map(str::to_string),
            port,
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![],
            health_check: None,
        }
    }

    #[test]
    fn validation_accepts_legacy_and_basic_multi_container() {
        validate_containers_and_routes(None, &[]).unwrap();
        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        let routes = vec![RouteSpec {
            path: "/".to_string(),
            container: "api".to_string(),
            access: None,
        }];
        validate_containers_and_routes(Some(&containers), &routes).unwrap();
    }

    #[test]
    fn validation_preserves_error_messages() {
        let containers = vec![cspec("API", Some("nginx"), Some(8080))];
        let err = validate_containers_and_routes(Some(&containers), &[]).unwrap_err();
        assert!(err.message.contains("Invalid container name"));

        let containers = vec![cspec("api", Some("nginx"), Some(8080))];
        let routes = vec![RouteSpec {
            path: "/.rise".to_string(),
            container: "api".to_string(),
            access: None,
        }];
        let err = validate_containers_and_routes(Some(&containers), &routes).unwrap_err();
        assert!(err.message.contains("reserved") && err.message.contains("/.rise"));
    }

    #[test]
    fn validation_rejects_secret_container_env_and_probe_without_port() {
        let mut spec = cspec("api", Some("nginx"), Some(8080));
        spec.env_overrides.push(EnvOverride {
            key: "API_KEY".to_string(),
            value: "secret".to_string(),
            is_secret: true,
            is_protected: None,
            source: None,
            for_environment: None,
        });
        let err = validate_containers_and_routes(Some(&[spec]), &[]).unwrap_err();
        assert!(err.message.contains("secret env overrides"));

        let mut spec = cspec("api", Some("nginx"), None);
        spec.health_check = Some(HealthCheckSpec::default());
        let err = validate_containers_and_routes(Some(&[spec]), &[]).unwrap_err();
        assert!(err.message.contains("health_check"));
    }

    #[test]
    fn project_config_validation_preserves_messages() {
        let mut config = crate::project_config::ProjectBuildConfig::default();
        config.routes.insert(
            "/".to_string(),
            crate::project_config::RouteConfig {
                container: "api".to_string(),
                access: None,
            },
        );
        let err = validate_project_config(&config).unwrap_err();
        assert!(err.message.contains("[routes] requires"));

        let mut config = crate::project_config::ProjectBuildConfig::default();
        config.containers.insert(
            "API".to_string(),
            crate::project_config::ContainerConfig {
                image: Some("nginx".to_string()),
                ..Default::default()
            },
        );
        let err = validate_project_config(&config).unwrap_err();
        assert!(err.message.contains("Invalid container name"));
    }
}
