// Project-level build configuration (rise.toml / .rise.toml)

use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// Re-export shared config types from rise_toml module
pub use crate::rise_toml::{ProjectBuildConfig, ProjectConfig};

/// Workspace-config filename. Lives at an ancestor directory of `rise.toml`
/// (typically the source-repo root) and provides defaults that apply to every
/// app under it — e.g. a shared `[registry] image_base` so apps don't repeat it.
pub const WORKSPACE_CONFIG_FILENAME: &str = "rise.workspace.toml";

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

/// Walk up from `app_path` looking for a `rise.workspace.toml`. The search
/// requires a `.git` marker somewhere in the ancestor chain — the workspace
/// file only counts when it lives at or above the repo root, and never above
/// `.git`. That way a stray `rise.workspace.toml` in `$HOME` or `/` can never
/// silently leak in, including on archive checkouts or CI clones that omit
/// `.git`.
///
/// Returns the first workspace file found within the walk (nearest-ancestor
/// wins). `app_path`'s own directory is searched too — a workspace file
/// colocated with `rise.toml` is unusual but not forbidden.
fn find_workspace_config(app_path: &Path) -> Option<PathBuf> {
    let mut current: PathBuf = if app_path.is_absolute() {
        app_path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(app_path)
    };
    // Resolve so the `.git` boundary check sees real ancestors.
    current = current.canonicalize().unwrap_or(current);

    let mut nearest_workspace: Option<PathBuf> = None;
    loop {
        if nearest_workspace.is_none() {
            let candidate = current.join(WORKSPACE_CONFIG_FILENAME);
            if candidate.is_file() {
                nearest_workspace = Some(candidate);
            }
        }
        if current.join(".git").exists() {
            return nearest_workspace;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None,
        }
    }
}

/// Load and parse a workspace config file at the given path. Same parsing
/// rules as `load_full_project_config` (TOML, version=1, unused-field warnings,
/// at-most-one default environment), since `rise.workspace.toml` uses the same
/// `ProjectBuildConfig` shape — it's defaults for any of the same fields.
fn load_workspace_config_at(path: &Path) -> Result<ProjectBuildConfig> {
    info!("Loading workspace config from {}", path.display());
    let content = std::fs::read_to_string(path)?;
    parse_project_config(&content, path)
}

/// Shared TOML parser used by both per-app and workspace loaders.
fn parse_project_config(content: &str, source_path: &Path) -> Result<ProjectBuildConfig> {
    let mut unused_fields = Vec::new();
    let deserializer = toml::Deserializer::parse(content)
        .map_err(|e| anyhow::Error::new(e).context("Failed to parse TOML"))?;
    let config: ProjectBuildConfig = serde_ignored::deserialize(deserializer, |path| {
        unused_fields.push(path.to_string());
    })?;

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

    Ok(config)
}

/// Merge a workspace config (defaults) with a per-app leaf config (overrides).
/// Top-level `Option` fields take the leaf when present, otherwise the
/// workspace. Per-environment configs are deep-merged field-by-field so a
/// leaf that only sets, say, `[environments.production.deploy]` doesn't
/// wipe the workspace's `[environments.production.registry]` — production
/// is exactly the environment where the registry override matters most.
fn merge_workspace_and_leaf(
    workspace: Option<ProjectBuildConfig>,
    leaf: Option<ProjectBuildConfig>,
) -> Option<ProjectBuildConfig> {
    match (workspace, leaf) {
        (None, leaf) => leaf,
        (Some(w), None) => Some(w),
        (Some(w), Some(l)) => {
            let mut environments = w.environments;
            for (name, leaf_env) in l.environments {
                match environments.remove(&name) {
                    Some(ws_env) => {
                        environments.insert(name, merge_environment(ws_env, leaf_env));
                    }
                    None => {
                        environments.insert(name, leaf_env);
                    }
                }
            }
            Some(ProjectBuildConfig {
                version: l.version.or(w.version),
                project: l.project.or(w.project),
                build: l.build.or(w.build),
                deploy: l.deploy.or(w.deploy),
                registry: l.registry.or(w.registry),
                environments,
            })
        }
    }
}

/// Merge a workspace's `EnvironmentConfig` with a leaf's for the same
/// environment name. Leaf wins for scalars and env-var map entries with
/// shared keys; workspace fields survive when the leaf doesn't touch them.
fn merge_environment(
    workspace: crate::rise_toml::EnvironmentConfig,
    leaf: crate::rise_toml::EnvironmentConfig,
) -> crate::rise_toml::EnvironmentConfig {
    let mut env = workspace.env;
    env.extend(leaf.env);
    crate::rise_toml::EnvironmentConfig {
        default: leaf.default || workspace.default,
        env,
        deploy: leaf.deploy.or(workspace.deploy),
        registry: leaf.registry.or(workspace.registry),
    }
}

/// Like `load_full_project_config`, but also walks up from `app_path` looking
/// for a `rise.workspace.toml` and merges it as defaults. Use this for
/// deploy/build/run paths so a source-repo-level config (e.g. shared
/// `[registry] image_base`) is inherited by every app under it.
///
/// `rise project create` deliberately does NOT use this — it must only see the
/// directory's own rise.toml when deciding whether to overwrite.
pub fn load_full_project_config_with_workspace(
    app_path: &str,
) -> Result<Option<ProjectBuildConfig>> {
    let leaf = load_full_project_config(app_path)?;
    let workspace = match find_workspace_config(Path::new(app_path)) {
        Some(path) => Some(load_workspace_config_at(&path)?),
        None => None,
    };
    Ok(merge_workspace_and_leaf(workspace, leaf))
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
                    registry: None,
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
    fn test_workspace_provides_registry_when_leaf_omits_it() {
        let tmp = tempfile::tempdir().unwrap();
        // Repo root marker + workspace config
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[registry]
image_base = "jfrog.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps"
"#,
        )
        .unwrap();
        // App directory with its own rise.toml that omits [registry]
        let app_dir = tmp.path().join("apps").join("compass");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "compass"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            merged.registry.as_ref().unwrap().image_base,
            "jfrog.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps"
        );
        assert_eq!(merged.project.unwrap().name, "compass");
    }

    #[test]
    fn test_leaf_overrides_workspace_registry() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[registry]
image_base = "workspace.example.com/base"
"#,
        )
        .unwrap();
        let app_dir = tmp.path().join("special");
        std::fs::create_dir(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "special"

[registry]
image_base = "leaf.example.com/override"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            merged.registry.unwrap().image_base,
            "leaf.example.com/override",
            "leaf must win over workspace when both set [registry]"
        );
    }

    #[test]
    fn test_workspace_lookup_stops_at_git_boundary() {
        // Outer "fake home" with a stray workspace config — must NOT be picked up.
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(
            outer.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[registry]
image_base = "should.not.be.inherited/from/home"
"#,
        )
        .unwrap();
        // Inner "source repo" — .git marker scopes the search.
        let repo = outer.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        let app_dir = repo.join("apps").join("a");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "a"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            merged.registry.is_none(),
            "workspace lookup escaped the repo via parent traversal"
        );
    }

    #[test]
    fn test_no_workspace_file_matches_load_full_project_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join("rise.toml"),
            r#"
[project]
name = "solo"
"#,
        )
        .unwrap();

        let baseline = load_full_project_config(tmp.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        let with_ws = load_full_project_config_with_workspace(tmp.path().to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            baseline.project.as_ref().unwrap().name,
            with_ws.project.as_ref().unwrap().name
        );
        assert!(with_ws.registry.is_none());
    }

    #[test]
    fn test_workspace_environments_merge_with_leaf_overriding() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[environments.staging.env]
SHARED = "from-workspace"
ONLY_WS = "ws-only"
"#,
        )
        .unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "app"

[environments.staging.env]
SHARED = "from-leaf"
ONLY_LEAF = "leaf-only"

[environments.production.env]
APP_VAR = "leaf-prod"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        // Deep-merged: leaf wins for SHARED, workspace's ONLY_WS survives.
        let staging = merged.environments.get("staging").unwrap();
        assert_eq!(staging.env.get("SHARED").unwrap(), "from-leaf");
        assert_eq!(staging.env.get("ONLY_LEAF").unwrap(), "leaf-only");
        assert_eq!(staging.env.get("ONLY_WS").unwrap(), "ws-only");
        // The `production` env exists only in the leaf — passes through.
        let production = merged.environments.get("production").unwrap();
        assert_eq!(production.env.get("APP_VAR").unwrap(), "leaf-prod");
    }

    #[test]
    fn test_leaf_env_does_not_shadow_workspace_registry() {
        // The core reason we deep-merge: a leaf touching production.env or
        // production.deploy must not silently wipe the workspace's
        // production.registry override (playground → snapshot for develop
        // deploys is the whole point).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[environments.production.registry]
image_base = "jfrog.example.com/snapshot"
"#,
        )
        .unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "app"

# Leaf touches production.env but must not wipe workspace's production.registry.
[environments.production.env]
APP_VAR = "prod-value"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        let production = merged.environments.get("production").unwrap();
        assert_eq!(
            production.registry.as_ref().unwrap().image_base,
            "jfrog.example.com/snapshot",
        );
        assert_eq!(production.env.get("APP_VAR").unwrap(), "prod-value");
    }

    #[test]
    fn test_workspace_lookup_requires_git_marker() {
        // Without a `.git` in the ancestor chain the workspace file must
        // NOT be honored — protects archive checkouts / CI clones without
        // `.git` from silently inheriting a stray `rise.workspace.toml` in
        // `$HOME` or `/`.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[registry]
image_base = "should.not.be.inherited/from/home"
"#,
        )
        .unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "app"
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            merged.registry.is_none(),
            "workspace file without a .git ancestor must not leak in",
        );
    }

    #[test]
    fn test_workspace_per_env_registry_passes_through_to_leaf() {
        // Workspace declares production-only registry override; leaf only
        // touches staging (the common rise-apps shape). The production
        // [registry] from workspace must survive the env-map union.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(WORKSPACE_CONFIG_FILENAME),
            r#"
[registry]
image_base = "jfrog.example.com/playground"

[environments.production.registry]
image_base = "jfrog.example.com/snapshot"
"#,
        )
        .unwrap();
        let app_dir = tmp.path().join("app");
        std::fs::create_dir(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("rise.toml"),
            r#"
[project]
name = "app"

[environments.staging]
default = true
"#,
        )
        .unwrap();

        let merged = load_full_project_config_with_workspace(app_dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            merged.registry.as_ref().unwrap().image_base,
            "jfrog.example.com/playground",
            "top-level registry default flows through"
        );
        let production = merged.environments.get("production").unwrap();
        assert_eq!(
            production.registry.as_ref().unwrap().image_base,
            "jfrog.example.com/snapshot",
            "per-env registry override survives env-map union when leaf doesn't \
             touch the same env"
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
