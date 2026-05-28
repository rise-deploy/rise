use anyhow::Result;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum BackendCommands {
    /// Start the HTTP server with all controllers
    #[cfg(feature = "backend")]
    Server,
    /// Check backend configuration for errors and unused options
    #[cfg(feature = "backend")]
    CheckConfig,
    /// Print backend settings JSON schema
    #[cfg(feature = "backend")]
    ConfigSchema,
    /// Print the rise.toml JSON schema
    #[cfg(feature = "backend")]
    RiseTomlSchema,
    /// Print the RiseProject CRD as YAML
    #[cfg(feature = "backend")]
    CrdSchema,
    /// Generate or print JSON schemas for the generic resource API.
    #[cfg(feature = "backend")]
    #[command(subcommand)]
    Schemas(SchemasCommands),
}

/// `rise backend schemas …` — JSON Schema artifacts for resource API types.
///
/// `rise.toml` and backend settings have dedicated top-level commands
/// (`rise-toml-schema`, `config-schema`) and are intentionally not generated
/// here, to avoid two code paths that could drift.
#[derive(Debug, Clone, clap::Subcommand)]
#[cfg(feature = "backend")]
pub enum SchemasCommands {
    /// Write deterministic JSON Schema files to a directory.
    ///
    /// Defaults to `docs/engineering/public/schemas/`, the location served
    /// by the operator docs site. Output is byte-identical across runs,
    /// so this command is safe to wire into a CI `check` task.
    Generate {
        /// Directory to write schema files into. Created if missing.
        #[arg(long, default_value = "docs/engineering/public/schemas")]
        out_dir: std::path::PathBuf,
    },
    /// Print the generated schemas to stdout (for inspection / piping).
    Print,
}

#[cfg(feature = "backend")]
pub async fn handle_backend_command(cmd: BackendCommands) -> Result<()> {
    match cmd {
        #[cfg(feature = "backend")]
        BackendCommands::Server => {
            let settings = crate::server::settings::Settings::new()?;
            crate::server::run_server(settings).await
        }
        #[cfg(feature = "backend")]
        BackendCommands::CheckConfig => {
            println!("Checking backend configuration...");
            match crate::server::settings::Settings::new() {
                Ok(_) => {
                    println!("✓ Configuration is valid");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("✗ Configuration error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        #[cfg(feature = "backend")]
        BackendCommands::ConfigSchema => {
            let schema = crate::server::settings::Settings::json_schema_value();
            println!("{}", serde_json::to_string_pretty(&schema)?);
            Ok(())
        }
        #[cfg(feature = "backend")]
        BackendCommands::RiseTomlSchema => {
            let schema = schemars::schema_for!(crate::rise_toml::ProjectBuildConfig);
            println!("{}", serde_json::to_string_pretty(&schema.to_value())?);
            Ok(())
        }
        #[cfg(feature = "backend")]
        BackendCommands::CrdSchema => {
            use kube::CustomResourceExt;
            let crd = crate::server::deployment::crd::RiseProject::crd();
            print!("{}", serde_yaml::to_string(&crd)?);
            Ok(())
        }
        #[cfg(feature = "backend")]
        BackendCommands::Schemas(sub) => handle_schemas_command(sub).await,
    }
}

#[cfg(feature = "backend")]
async fn handle_schemas_command(cmd: SchemasCommands) -> Result<()> {
    use crate::server::resources::schemas;

    match cmd {
        SchemasCommands::Generate { out_dir } => {
            let written = schemas::write_to_dir(&out_dir)?;
            for path in written {
                println!("{}", path.display());
            }
            Ok(())
        }
        SchemasCommands::Print => {
            schemas::print_to_stdout()?;
            Ok(())
        }
    }
}
