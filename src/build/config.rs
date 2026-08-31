// Project-level build configuration (rise.toml / .rise.toml)

use anyhow::Result;
use std::path::Path;
use tracing::{debug, info, warn};

// Re-export shared config types from rise_toml module
pub use crate::rise_toml::{ProjectBuildConfig, ProjectConfig};

/// Load full project configuration from rise.toml or .rise.toml
///
/// Searches for rise.toml first, then .rise.toml in the given directory.
/// Returns Ok(None) if no config file is found.
/// Returns Err if file exists but cannot be read or parsed, or if version is unsupported.
pub fn load_full_project_config(app_path: &str) -> Result<Option<ProjectBuildConfig>> {
    let rise_toml = Path::new(app_path).join("rise.toml");
    let dot_rise_toml = Path::new(app_path).join(".rise.toml");

    // Warn if both files exist
    if rise_toml.exists() && dot_rise_toml.exists() {
        warn!("Both rise.toml and .rise.toml found. Using rise.toml.");
    }

    // Determine which config file to use
    let config_path = if rise_toml.exists() {
        Some(rise_toml)
    } else if dot_rise_toml.exists() {
        Some(dot_rise_toml)
    } else {
        None
    };

    // Parse if found
    if let Some(path) = config_path {
        info!("Loading project config from {}", path.display());
        let content = std::fs::read_to_string(&path)?;

        // Deserialize and collect any unused fields
        let mut unused_fields = Vec::new();
        let deserializer = toml::Deserializer::parse(&content)
            .map_err(|e| anyhow::Error::new(e).context("Failed to parse TOML"))?;
        let config: ProjectBuildConfig = serde_ignored::deserialize(deserializer, |path| {
            unused_fields.push(path.to_string());
        })?;

        // Warn about unused fields
        for field in &unused_fields {
            warn!(
                "Unknown configuration field in {}: {}",
                path.display(),
                field
            );
        }

        // Validate version
        if let Some(version) = config.version {
            if version != 1 {
                anyhow::bail!(
                    "Unsupported rise.toml version: {}. This CLI supports version 1.",
                    version
                );
            }
        } else {
            debug!("No version specified in rise.toml, using latest");
        }

        // Validate at most one environment has default = true
        let default_envs: Vec<&str> = config
            .environments
            .iter()
            .filter(|(_, env)| env.default)
            .map(|(name, _)| name.as_str())
            .collect();
        if default_envs.len() > 1 {
            anyhow::bail!(
                "Multiple environments have default = true: {}. Only one is allowed.",
                default_envs.join(", ")
            );
        }

        rise_deployment_spec::validation::validate_project_config(&config)
            .map_err(|e| anyhow::anyhow!(e.message))?;

        Ok(Some(config))
    } else {
        Ok(None)
    }
}

/// Write project configuration to rise.toml
///
/// Creates or overwrites rise.toml in the specified directory.
pub fn write_project_config(app_path: &str, config: &ProjectBuildConfig) -> Result<()> {
    let rise_toml_path = Path::new(app_path).join("rise.toml");
    let toml_string = toml::to_string_pretty(config)?;
    std::fs::write(&rise_toml_path, toml_string)?;
    info!("Wrote project config to {}", rise_toml_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rise_toml::EnvironmentConfig;
    use std::collections::BTreeMap;

    /// A resource key put on the container table instead of its `deploy` table
    /// parses fine and does nothing, which is the shape of mistake most likely
    /// to be made and least likely to be noticed. The path in the warning has
    /// to name the container, or it does not help.
    #[test]
    fn a_misplaced_per_container_key_is_reported_with_its_path() {
        let toml = r#"
version = 1

[project]
name = "test-project"

[containers.web]
port = 80
replicas = 3

[containers.web.deploy]
cpu = "250m"
"#;
        let mut ignored = Vec::new();
        let deserializer = toml::Deserializer::parse(toml).unwrap();
        let config: ProjectBuildConfig =
            serde_ignored::deserialize(deserializer, |path| ignored.push(path.to_string()))
                .expect("an unknown key is ignored, not fatal");

        assert_eq!(
            ignored,
            vec!["containers.web.replicas".to_string()],
            "the warning must say which container and which key",
        );
        // And the correctly-placed sibling still took effect, so the report is
        // about the stray key rather than the whole table being dropped.
        assert_eq!(
            config.containers["web"]
                .deploy
                .as_ref()
                .and_then(|d| d.cpu.as_deref()),
            Some("250m"),
        );
    }

    #[test]
    fn test_load_config_with_unused_fields() {
        // Create a temporary directory for test
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        // Write a config with some unknown fields
        std::fs::write(
            &rise_toml_path,
            r#"
version = 1

[project]
name = "test-project"

[build]
backend = "docker"

# Unknown fields that should trigger warnings
unknown_field = "test"
another_unknown = 123

[unknown_section]
foo = "bar"
"#,
        )
        .unwrap();

        // Load the config - it should succeed despite unknown fields
        let result = load_full_project_config(temp_dir.path().to_str().unwrap());

        assert!(result.is_ok(), "Config should load despite unknown fields");
        let config = result.unwrap();
        assert!(config.is_some(), "Config should be present");

        let config = config.unwrap();
        assert_eq!(config.version, Some(1));
        assert!(config.project.is_some());
        assert_eq!(config.project.unwrap().name, "test-project");
    }

    #[test]
    fn test_load_config_with_environments() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        std::fs::write(
            &rise_toml_path,
            r#"
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
DATABASE_URL = "postgres://localhost/mydb"

[environments.staging.env]
DATABASE_URL = "postgres://staging-db/mydb"
LOG_LEVEL = "debug"

[environments.production.env]
DATABASE_URL = "postgres://prod-db/mydb"
"#,
        )
        .unwrap();

        let result = load_full_project_config(temp_dir.path().to_str().unwrap());
        assert!(result.is_ok());
        let config = result.unwrap().unwrap();

        // Check global env
        let project = config.project.as_ref().unwrap();
        assert_eq!(project.env.get("LOG_LEVEL").unwrap(), "info");
        assert_eq!(
            project.env.get("DATABASE_URL").unwrap(),
            "postgres://localhost/mydb"
        );

        // Check environments
        assert_eq!(config.environments.len(), 2);

        let staging = config.environments.get("staging").unwrap();
        assert_eq!(
            staging.env.get("DATABASE_URL").unwrap(),
            "postgres://staging-db/mydb"
        );
        assert_eq!(staging.env.get("LOG_LEVEL").unwrap(), "debug");

        let production = config.environments.get("production").unwrap();
        assert_eq!(
            production.env.get("DATABASE_URL").unwrap(),
            "postgres://prod-db/mydb"
        );
        assert!(!production.env.contains_key("LOG_LEVEL"));
    }

    #[test]
    fn test_load_config_without_environments() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        std::fs::write(
            &rise_toml_path,
            r#"
[project]
name = "no-envs"

[project.env]
FOO = "bar"
"#,
        )
        .unwrap();

        let result = load_full_project_config(temp_dir.path().to_str().unwrap());
        assert!(result.is_ok());
        let config = result.unwrap().unwrap();
        assert!(config.environments.is_empty());
    }

    #[test]
    fn test_roundtrip_config_with_environments() {
        let config = ProjectBuildConfig {
            version: Some(1),
            project: Some(ProjectConfig {
                name: "roundtrip-app".to_string(),
                env: BTreeMap::from([("GLOBAL".to_string(), "val".to_string())]),
            }),
            build: None,
            deploy: None,
            identity: None,
            environments: BTreeMap::from([(
                "staging".to_string(),
                EnvironmentConfig {
                    default: true,
                    env: BTreeMap::from([("STAGE_VAR".to_string(), "stage_val".to_string())]),
                    deploy: None,
                },
            )]),
            containers: BTreeMap::new(),
            routes: BTreeMap::new(),
        };

        let temp_dir = tempfile::tempdir().unwrap();
        write_project_config(temp_dir.path().to_str().unwrap(), &config).unwrap();

        let loaded = load_full_project_config(temp_dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, Some(1));
        assert_eq!(loaded.project.as_ref().unwrap().name, "roundtrip-app");
        assert_eq!(
            loaded.project.as_ref().unwrap().env.get("GLOBAL").unwrap(),
            "val"
        );
        let staging = loaded.environments.get("staging").unwrap();
        assert!(staging.default);
        assert_eq!(staging.env.get("STAGE_VAR").unwrap(), "stage_val");
    }

    #[test]
    fn test_load_config_without_unknown_fields() {
        // Create a temporary directory for test
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        // Write a clean config
        std::fs::write(
            &rise_toml_path,
            r#"
version = 1

[project]
name = "clean-project"

[build]
backend = "pack"
builder = "heroku/builder:24"
"#,
        )
        .unwrap();

        // Load the config - should work fine
        let result = load_full_project_config(temp_dir.path().to_str().unwrap());

        assert!(result.is_ok());
        let config = result.unwrap();
        assert!(config.is_some());

        let config = config.unwrap();
        assert_eq!(config.version, Some(1));
        assert!(config.project.is_some());
        assert_eq!(config.project.as_ref().unwrap().name, "clean-project");
    }

    #[test]
    fn test_load_config_rejects_multiple_defaults() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        std::fs::write(
            &rise_toml_path,
            r#"
[project]
name = "multi-default"

[environments.staging]
default = true

[environments.production]
default = true
"#,
        )
        .unwrap();

        let result = load_full_project_config(temp_dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Multiple environments have default = true"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_load_config_with_default_environment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rise_toml_path = temp_dir.path().join("rise.toml");

        std::fs::write(
            &rise_toml_path,
            r#"
[project]
name = "default-env"

[environments.staging]
default = true
env.NODE_ENV = "development"

[environments.production]
env.NODE_ENV = "production"
"#,
        )
        .unwrap();

        let result = load_full_project_config(temp_dir.path().to_str().unwrap());
        assert!(result.is_ok());
        let config = result.unwrap().unwrap();
        let staging = config.environments.get("staging").unwrap();
        assert!(staging.default);
        let production = config.environments.get("production").unwrap();
        assert!(!production.default);
    }

    fn write_toml(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rise.toml"), contents).unwrap();
        dir
    }

    #[test]
    fn test_multi_container_basic() {
        let dir = write_toml(
            r#"
[project]
name = "my-app"

[containers.frontend]
image = "registry.example.com/myapp/frontend:latest"
port = 8080
[containers.frontend.deploy]
replicas = 2

[containers.backend]
image = "registry.example.com/myapp/backend:latest"
port = 9090
[containers.backend.deploy]
replicas = 3
health_check = { path = "/health" }

[containers.worker]
image = "registry.example.com/myapp/worker:latest"

[routes]
"/api" = { container = "backend" }
"/" = { container = "frontend" }
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(config.containers.len(), 3);
        assert_eq!(config.routes.len(), 2);
        let resolved = config.resolve_deploy().unwrap();
        assert_eq!(resolved.containers.len(), 3);
        assert_eq!(resolved.routes.len(), 2);
        let api_route = resolved.routes.iter().find(|r| r.path == "/api").unwrap();
        assert_eq!(api_route.container, "backend");
    }

    #[test]
    fn test_multi_container_top_level_build_deploy_as_defaults() {
        // Top-level [build]/[deploy] now coexist with [containers] and act as
        // per-field defaults that each container inherits.
        let dir = write_toml(
            r#"
[build]
backend = "docker"

[deploy]
replicas = 2
cpu = "250m"

[containers.web]
port = 8080
[containers.web.build]
dockerfile = "web/Dockerfile"

[containers.worker]
[containers.worker.deploy]
cpu = "500m"
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        let resolved = config.resolve_deploy().unwrap();

        let web = resolved
            .containers
            .iter()
            .find(|c| c.name == "web")
            .unwrap();
        let web_build = web.build.as_ref().expect("web inherits a build");
        assert_eq!(web_build.backend.as_deref(), Some("docker")); // inherited
        assert_eq!(web_build.dockerfile.as_deref(), Some("web/Dockerfile")); // own
        assert_eq!(web.replicas, Some(2)); // inherited deploy
        assert_eq!(web.cpu.as_deref(), Some("250m")); // inherited deploy

        let worker = resolved
            .containers
            .iter()
            .find(|c| c.name == "worker")
            .unwrap();
        assert_eq!(
            worker.build.as_ref().and_then(|b| b.backend.as_deref()),
            Some("docker") // worker has no own build → inherits the whole top-level build
        );
        assert_eq!(worker.cpu.as_deref(), Some("500m")); // own override
        assert_eq!(worker.replicas, Some(2)); // inherited deploy
    }

    #[test]
    fn test_multi_container_rejects_route_to_unknown_container() {
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080

[routes]
"/" = { container = "frontend" }
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected unknown-container error");
        assert!(
            err.to_string().contains("unknown container 'frontend'"),
            "got: {err}"
        );
    }

    #[test]
    fn test_multi_container_rejects_route_to_worker() {
        // Worker = container without a port = not routable.
        let dir = write_toml(
            r#"
[containers.worker]
image = "foo:bar"

[routes]
"/" = { container = "worker" }
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected missing-port error");
        assert!(err.to_string().contains("no port"), "got: {err}");
    }

    #[test]
    fn test_health_check_false_disables_probes() {
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080
[containers.api.deploy]
health_check = false
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        let api = config.containers.get("api").unwrap();
        assert!(matches!(
            api.deploy.as_ref().and_then(|d| d.health_check.as_ref()),
            Some(crate::rise_toml::HealthCheckSetting::Disabled(_))
        ));
    }

    #[test]
    fn test_health_check_true_is_rejected() {
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080
[containers.api.deploy]
health_check = true
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected health_check=true to be rejected");
        // Untagged enum: the message indicates no variant matched. (BoolFalse
        // rejects `true`; the table variant doesn't match a scalar bool.)
        let msg = err.to_string();
        assert!(
            msg.contains("HealthCheckSetting") || msg.contains("not allowed"),
            "got: {msg}"
        );
    }

    #[test]
    fn test_default_route_synthesised_for_single_routable_container() {
        // When only one container has http_port and [routes] is omitted,
        // resolve_deploy should synthesise `/` → that container.
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080

[containers.worker]
image = "bar:baz"
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        let resolved = config.resolve_deploy().unwrap();
        assert_eq!(resolved.routes.len(), 1);
        assert_eq!(resolved.routes[0].path, "/");
        assert_eq!(resolved.routes[0].container, "api");
    }

    #[test]
    fn test_single_container_config_resolves_empty() {
        // A single-container project (top-level [build]/[deploy], no [containers])
        // resolves to no explicit containers — the CLI drives it through the
        // single-container build flow and the backend synthesises the implicit
        // `app` container at reconcile time.
        let dir = write_toml(
            r#"
[project]
name = "simple"

[build]
backend = "docker"

[deploy]
replicas = 2
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(config.containers.is_empty());
        let resolved = config.resolve_deploy().unwrap();
        assert!(resolved.containers.is_empty());
        assert!(resolved.routes.is_empty());
    }

    #[test]
    fn test_invalid_container_name() {
        let dir = write_toml(
            r#"
[containers."Bad Name"]
image = "foo:bar"
port = 8080
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected invalid-name error");
        assert!(
            err.to_string().contains("Invalid container name"),
            "got: {err}"
        );
    }

    #[test]
    fn test_container_rejects_both_image_and_build() {
        let dir = write_toml(
            r#"
[containers.app]
image = "foo:bar"
port = 8080

[containers.app.build]
backend = "docker"
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected image+build mutual-exclusion error");
        assert!(
            err.to_string()
                .contains("cannot set both 'image' and [build]"),
            "got: {err}"
        );
    }

    #[test]
    fn test_container_rejects_neither_image_nor_build() {
        let dir = write_toml(
            r#"
[containers.app]
port = 8080
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected missing-image-or-build error");
        assert!(
            err.to_string()
                .contains("must set either 'image' or [build]"),
            "got: {err}"
        );
    }

    #[test]
    fn test_container_health_check_requires_http_port() {
        let dir = write_toml(
            r#"
[containers.app]
image = "foo:bar"
[containers.app.deploy]
health_check = { path = "/health" }
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected health_check-without-port error");
        assert!(
            err.to_string().contains("health_check but no port"),
            "got: {err}"
        );
    }

    #[test]
    fn test_route_path_missing_leading_slash_fails() {
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080

[routes]
"api" = { container = "api" }
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("expected route path leading-slash error");
        assert!(
            err.to_string().contains("must start with '/'"),
            "got: {err}"
        );
    }

    #[test]
    fn test_route_path_reserved_rise_prefix_rejected() {
        for reserved in ["/.rise", "/.rise/auth"] {
            let dir = write_toml(&format!(
                r#"
[containers.api]
image = "foo:bar"
port = 8080

[routes]
"{reserved}" = {{ container = "api" }}
"#,
            ));
            let err = load_full_project_config(dir.path().to_str().unwrap())
                .expect_err("expected reserved-prefix error");
            assert!(
                err.to_string().contains("reserved") && err.to_string().contains("/.rise"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn test_route_path_normal_accepted() {
        let dir = write_toml(
            r#"
[containers.api]
image = "foo:bar"
port = 8080

[routes]
"/api" = { container = "api" }
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .expect("normal /api path should be accepted")
            .unwrap();
        assert!(config.routes.contains_key("/api"));
    }

    #[test]
    fn test_route_path_invalid_charset_rejected() {
        // A space and a ';' are both outside the allowed URL-path charset
        // (and are valid inside a quoted TOML key, so parsing reaches the
        // validator).
        for bad in ["/has space", "/has;semicolon"] {
            let dir = write_toml(&format!(
                r#"
[containers.api]
image = "foo:bar"
port = 8080

[routes]
"{bad}" = {{ container = "api" }}
"#,
            ));
            let err = load_full_project_config(dir.path().to_str().unwrap())
                .expect_err("expected invalid-charset error");
            assert!(err.to_string().contains("invalid characters"), "got: {err}");
        }
    }

    #[test]
    fn test_container_name_15_chars_ok() {
        let dir = write_toml(
            r#"
[containers.abcdefghijklmno]
image = "foo:bar"
port = 8080
"#,
        );
        let config = load_full_project_config(dir.path().to_str().unwrap())
            .expect("15-char container name should be accepted")
            .unwrap();
        assert!(config.containers.contains_key("abcdefghijklmno"));
    }

    #[test]
    fn test_container_name_16_chars_fails() {
        let dir = write_toml(
            r#"
[containers.abcdefghijklmnop]
image = "foo:bar"
port = 8080
"#,
        );
        let err = load_full_project_config(dir.path().to_str().unwrap())
            .expect_err("16-char container name should be rejected");
        assert!(
            err.to_string().contains("Invalid container name"),
            "got: {err}"
        );
    }
}
