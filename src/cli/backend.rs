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
    }
}
