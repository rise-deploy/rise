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

    if let Some(path) = config_path {
        info!("Loading project config from {}", path.display());
        let content = std::fs::read_to_string(&path)?;
        Ok(Some(parse_project_config(&content, &path)?))
    } else {
        Ok(None)
    }
}

fn parse_project_config(content: &str, source_path: &Path) -> Result<ProjectBuildConfig> {
    let mut unused_fields = Vec::new();
    let deserializer = toml::Deserializer::parse(content)
        .map_err(|e| anyhow::Error::new(e).context("Failed to parse TOML"))?;
    let mut config: ProjectBuildConfig = serde_ignored::deserialize(deserializer, |path| {
        unused_fields.push(path.to_string());
    })?;

    if let Some(reg) = config.registry.as_mut() {
        reg.image_base = reg.image_base.trim().to_string();
    }

    for field in &unused_fields {
        warn!(
            "Unknown configuration field in {}: {}",
            source_path.display(),
            field
        );
    }

    if let Some(version) = config.version {
        if version != 1 {
            anyhow::bail!(
                "Unsupported {} version: {}. This CLI supports version 1.",
                source_path.display(),
                version
            );
        }
    } else {
        debug!(
            "No version specified in {}, using latest",
            source_path.display()
        );
    }

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

    if let Some(reg) = config.registry.as_ref() {
        validate_registry_config(reg, source_path)?;
    }

    Ok(config)
}

/// Validate a `[registry]` block. Rejects obvious mistakes at load time so
/// we don't blow up deep inside `docker push` with a garbled image ref.
fn validate_registry_config(
    reg: &crate::rise_toml::RegistryConfig,
    source_path: &Path,
) -> Result<()> {
    let image_base = &reg.image_base;
    if image_base.is_empty() {
        anyhow::bail!(
            "[registry] in {}: image_base must be a non-empty host/path prefix (e.g. \"jfrog.example.com/team/apps\")",
            source_path.display(),
        );
    }
    // Must contain at least one `/` separating host from path — a bare host
    // with no path prefix isn't a valid push target for `{host}/{project}:{id}`.
    if !image_base.contains('/') {
        anyhow::bail!(
            "[registry] in {}: image_base must contain a `/` between host and path prefix (got '{}')",
            source_path.display(),
            image_base,
        );
    }
    Ok(())
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
            registry: None,
            environments: BTreeMap::from([(
                "staging".to_string(),
                EnvironmentConfig {
                    default: true,
                    env: BTreeMap::from([("STAGE_VAR".to_string(), "stage_val".to_string())]),
                    deploy: None,
                },
            )]),
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
}
