//! Startup-only conversion of the accepted persisted credential baseline.
// Remove once every environment has completed the v2 credential baseline migration.

use super::{EncryptionContext, EncryptionProvider};
use crate::db::{encryption_migration::CredentialMigration, models::ProjectExtension};
use crate::server::extensions::providers::oauth::{
    models::OAuthExtensionSpec, provider::oauth_client_secret_context,
};
use crate::server::settings::{ExtensionProviderConfig, Settings};
use anyhow::{Context, Result};
use rise_backend_core::encryption::{pointer_segment, ENVELOPE_PREFIX};
use serde_json::Value;
use sqlx::PgPool;

/// The durable marker permits baseline import once. Subsequent startups do not
/// inspect caller-editable records; consumers reject invalid ciphertext locally.
pub async fn migrate(
    pool: &PgPool,
    provider: Option<&dyn EncryptionProvider>,
    settings: &Settings,
) -> Result<()> {
    let mut migration = CredentialMigration::begin(pool).await?;
    if migration.completed().await? {
        return Ok(());
    }

    required_provider(provider)?;
    let canary = migration_provider_guard(&mut migration, provider).await?;

    let mut after = None;
    loop {
        let page = migration.project_variables(after).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|var| var.id);
        for var in page {
            let context =
                EncryptionContext::project_env(var.project_id, var.environment_id, &var.key);
            let key = format!("project_env_vars:{}", var.id);
            if migration
                .progressed(&key, &fingerprint(&(&context, &var.value)))
                .await?
            {
                continue;
            }
            let value = rebind(provider, &var.value, &context)
                .await
                .with_context(|| format!("Project variable {} slot {}", var.project_id, var.key))?;
            migration
                .update_project_value(&var, &value, &key, &fingerprint(&(&context, &value)))
                .await?;
        }
    }
    let mut after = None;
    loop {
        let page = migration.deployment_variables(after).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|var| var.id);
        for var in page {
            let context = EncryptionContext::deployment_env(var.deployment_id, &var.key);
            let key = format!("deployment_env_vars:{}", var.id);
            if migration
                .progressed(&key, &fingerprint(&(&context, &var.value)))
                .await?
            {
                continue;
            }
            let value = rebind(provider, &var.value, &context)
                .await
                .with_context(|| {
                    format!("Deployment variable {} slot {}", var.deployment_id, var.key)
                })?;
            migration
                .update_deployment_value(&var, &value, &key, &fingerprint(&(&context, &value)))
                .await?;
        }
    }
    let mut after: Option<(uuid::Uuid, String)> = None;
    loop {
        let page = migration
            .extensions(
                after
                    .as_ref()
                    .map(|(project, extension)| (*project, extension.as_str())),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        after = page
            .last()
            .map(|ext| (ext.project_id, ext.extension.clone()));
        for mut ext in page {
            let source = ext.clone();
            let managed = managed_snowflake_binding(&mut migration, &ext, settings).await?;
            let key = format!("project_extensions:{}:{}", ext.project_id, ext.extension);
            if migration
                .progressed(&key, &extension_fingerprint(&ext, &managed))
                .await?
            {
                continue;
            }
            // Remove once every environment has completed the v2 baseline migration.
            if ext.extension_type == "oauth"
                && ext
                    .spec
                    .get("client_secret_encrypted")
                    .is_none_or(Value::is_null)
            {
                if let Some(name) = ext.spec.get("client_secret_ref").and_then(Value::as_str) {
                    let var = migration
                        .reference_variable(ext.project_id, name)
                        .await?
                        .context("OAuth baseline references a missing global variable")?;
                    let plaintext = if var.is_secret {
                        required_provider(provider)?
                            .decrypt(
                                &var.value,
                                &EncryptionContext::project_env(
                                    var.project_id,
                                    var.environment_id,
                                    &var.key,
                                ),
                            )
                            .await?
                    } else {
                        var.value
                    };
                    let context =
                        oauth_context(&ext, settings, &managed)
                            .await
                            .with_context(|| {
                                format!(
                                    "Extension {}:{} slot /spec/client_secret_encrypted",
                                    ext.project_id, ext.extension
                                )
                            })?;
                    ext.spec["client_secret_encrypted"] = Value::String(
                        required_provider(provider)?
                            .encrypt(&plaintext, &context)
                            .await?,
                    );
                }
            }
            if ext.extension_type == "oauth" {
                if let Some(object) = ext.spec.as_object_mut() {
                    object.remove("client_secret_ref");
                }
            }
            let oauth_context = if ext.extension_type == "oauth"
                && ext
                    .spec
                    .get("client_secret_encrypted")
                    .is_some_and(Value::is_string)
            {
                Some(
                    oauth_context(&ext, settings, &managed)
                        .await
                        .with_context(|| {
                            format!(
                                "Extension {}:{} slot /spec/client_secret_encrypted",
                                ext.project_id, ext.extension
                            )
                        })?,
                )
            } else {
                None
            };
            let base = EncryptionContext::extension(
                ext.project_id,
                &ext.extension_type,
                &ext.extension,
                "",
            );
            for (is_spec, root) in [(true, "/spec"), (false, "/status")] {
                let value = if is_spec { &ext.spec } else { &ext.status };
                let mut slots = Vec::new();
                encrypted_slots(value, root, &mut slots).with_context(|| {
                    format!(
                        "Invalid encrypted field in {}:{}",
                        ext.project_id, ext.extension
                    )
                })?;
                for (slot, ciphertext) in slots {
                    let context = if slot == "/spec/client_secret_encrypted"
                        && ext.extension_type == "oauth"
                    {
                        oauth_context
                            .clone()
                            .expect("OAuth string credential has a context")
                    } else {
                        base.with_slot(&slot)
                    };
                    let rebound =
                        rebind(provider, &ciphertext, &context)
                            .await
                            .with_context(|| {
                                format!(
                                    "Extension {}:{} slot {}",
                                    ext.project_id, ext.extension, slot
                                )
                            })?;
                    let pointer = slot.strip_prefix(root).expect("slot starts at its root");
                    let value = if is_spec {
                        &mut ext.spec
                    } else {
                        &mut ext.status
                    };
                    *value
                        .pointer_mut(pointer)
                        .context("Credential slot disappeared")? = Value::String(rebound);
                }
            }
            migration
                .update_extension(&source, &ext, &key, &extension_fingerprint(&ext, &managed))
                .await?;
        }
    }
    if let Some(canary) = canary {
        validate_provider_guard(required_provider(provider)?, &canary).await?;
    }
    migration.lock_for_validation().await?;
    validate_coverage(&mut migration, settings).await?;
    migration.finish().await?;
    tracing::info!("Credential context migration v2 completed before controller startup");
    Ok(())
}

/// Checkpoints cover every currently required record while table writes are excluded.
/// This pass performs no credential authentication, sealing, or endpoint discovery.
async fn validate_coverage(migration: &mut CredentialMigration, settings: &Settings) -> Result<()> {
    let mut after = None;
    loop {
        let page = migration.project_variables(after).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|var| var.id);
        let required = page
            .iter()
            .map(|var| {
                let context =
                    EncryptionContext::project_env(var.project_id, var.environment_id, &var.key);
                (
                    format!("project_env_vars:{}", var.id),
                    fingerprint(&(&context, &var.value)),
                )
            })
            .collect::<Vec<_>>();
        require_checkpoints(migration, required).await?;
    }
    let mut after = None;
    loop {
        let page = migration.deployment_variables(after).await?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|var| var.id);
        let required = page
            .iter()
            .map(|var| {
                let context = EncryptionContext::deployment_env(var.deployment_id, &var.key);
                (
                    format!("deployment_env_vars:{}", var.id),
                    fingerprint(&(&context, &var.value)),
                )
            })
            .collect::<Vec<_>>();
        require_checkpoints(migration, required).await?;
    }
    let mut after: Option<(uuid::Uuid, String)> = None;
    loop {
        let page = migration
            .extensions(
                after
                    .as_ref()
                    .map(|(project, extension)| (*project, extension.as_str())),
            )
            .await?;
        if page.is_empty() {
            break;
        }
        after = page
            .last()
            .map(|ext| (ext.project_id, ext.extension.clone()));
        let mut parents = migration.snowflake_parents(&page).await?;
        let mut required = Vec::with_capacity(page.len());
        for ext in page {
            let managed = managed_snowflake_binding_from_parent(
                &ext,
                settings,
                parents.remove(&(ext.project_id, ext.extension.clone())),
            )?;
            required.push((
                format!("project_extensions:{}:{}", ext.project_id, ext.extension),
                extension_fingerprint(&ext, &managed),
            ));
        }
        require_checkpoints(migration, required).await?;
    }
    Ok(())
}

async fn require_checkpoints(
    migration: &mut CredentialMigration,
    required: Vec<(String, String)>,
) -> Result<()> {
    let keys = required
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let checkpoints = migration.checkpoints(&keys).await?;
    for (key, fingerprint) in required {
        anyhow::ensure!(checkpoints.get(&key) == Some(&fingerprint), "Credential migration coverage changed for {key}; stop concurrent writes and retry startup");
    }
    Ok(())
}

const PROVIDER_CANARY: &str = "rise-credential-context-migration-v2";

async fn validate_provider_guard(
    provider: &dyn EncryptionProvider,
    ciphertext: &str,
) -> Result<()> {
    let value = provider
        .decrypt(
            ciphertext,
            &EncryptionContext::internal("credential-migration-v2/provider-canary"),
        )
        .await
        .context(
            "The migration encryption provider/key does not match its committed checkpoints",
        )?;
    anyhow::ensure!(
        value == PROVIDER_CANARY,
        "Invalid migration provider canary"
    );
    Ok(())
}

async fn migration_provider_guard(
    migration: &mut CredentialMigration,
    provider: Option<&dyn EncryptionProvider>,
) -> Result<Option<String>> {
    let canary = migration.provider_canary().await?;
    if migration.has_progress().await? {
        let canary = canary.context("Credential migration checkpoints have no provider canary")?;
        validate_provider_guard(required_provider(provider)?, &canary).await?;
        return Ok(Some(canary));
    }
    // No record has committed: there is no key-dependent progress to preserve.
    let Some(provider) = provider else {
        anyhow::ensure!(
            canary.is_none(),
            "An incomplete credential migration requires an encryption provider"
        );
        return Ok(None);
    };
    let ciphertext = provider
        .encrypt(
            PROVIDER_CANARY,
            &EncryptionContext::internal("credential-migration-v2/provider-canary"),
        )
        .await?;
    migration.save_provider_canary(&ciphertext).await?;
    Ok(Some(ciphertext))
}

fn fingerprint(value: &impl serde::Serialize) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&("rise-credential-migration-v2", value))
        .expect("persisted JSON values serialize");
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}

fn extension_fingerprint(ext: &ProjectExtension, managed: &Option<(String, String)>) -> String {
    fingerprint(&(
        ext.project_id,
        &ext.extension_type,
        &ext.extension,
        &ext.spec,
        &ext.status,
        managed.as_ref(),
    ))
}

fn required_provider(provider: Option<&dyn EncryptionProvider>) -> Result<&dyn EncryptionProvider> {
    provider.context("Persisted credentials require an encryption provider")
}

async fn rebind(
    provider: Option<&dyn EncryptionProvider>,
    value: &str,
    context: &EncryptionContext,
) -> Result<String> {
    let provider = required_provider(provider)?;
    if value.starts_with(ENVELOPE_PREFIX) {
        provider.decrypt(value, context).await?;
        Ok(value.into())
    } else {
        let plaintext = provider.decrypt_legacy_for_migration(value).await?;
        provider.encrypt(&plaintext, context).await
    }
}

fn encrypted_slots(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let pointer = format!("{prefix}/{}", pointer_segment(key));
                if key.ends_with("_encrypted") {
                    if let Some(ciphertext) = child.as_str() {
                        out.push((pointer, ciphertext.into()));
                    } else if !child.is_null() {
                        anyhow::bail!("Encrypted slot {pointer} must contain a string or null");
                    }
                } else {
                    encrypted_slots(child, &pointer, out)?;
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                encrypted_slots(child, &format!("{prefix}/{index}"), out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn snowflake_account(settings: &Settings, project: uuid::Uuid, parent: &str) -> Result<String> {
    settings
        .extensions
        .as_ref()
        .into_iter()
        .flat_map(|e| &e.providers)
        .rev()
        .find_map(|config| {
            if let ExtensionProviderConfig::SnowflakeOAuthProvisioner { account, .. } = config {
                Some(account.clone())
            } else {
                None
            }
        })
        .with_context(|| format!("Snowflake extension {project}:{parent} requires its configured provisioner account"))
}

async fn managed_snowflake_binding(
    migration: &mut CredentialMigration,
    ext: &ProjectExtension,
    settings: &Settings,
) -> Result<Option<(String, String)>> {
    let parent = migration
        .snowflake_parents(std::slice::from_ref(ext))
        .await?
        .remove(&(ext.project_id, ext.extension.clone()));
    managed_snowflake_binding_from_parent(ext, settings, parent)
}

fn managed_snowflake_binding_from_parent(
    ext: &ProjectExtension,
    settings: &Settings,
    parent: Option<(String, String)>,
) -> Result<Option<(String, String)>> {
    if ext.extension_type == "snowflake-oauth-provisioner"
        && ext
            .status
            .get("oauth_extension_name")
            .is_some_and(Value::is_string)
        && ext
            .status
            .get("oauth_client_id")
            .is_some_and(Value::is_string)
    {
        snowflake_account(settings, ext.project_id, &ext.extension)?;
    }
    parent
        .map(|(parent, client_id)| {
            Ok((
                snowflake_account(settings, ext.project_id, &parent)?,
                client_id,
            ))
        })
        .transpose()
}

async fn oauth_context(
    ext: &ProjectExtension,
    settings: &Settings,
    managed: &Option<(String, String)>,
) -> Result<EncryptionContext> {
    let mut spec: OAuthExtensionSpec =
        serde_json::from_value(ext.spec.clone()).with_context(|| {
            format!(
                "Invalid OAuth baseline {}:{}",
                ext.project_id, ext.extension
            )
        })?;
    if let Some((account, client_id)) = managed.as_ref() {
        spec.client_id = client_id.clone();
        spec.issuer_url = format!("https://{account}.snowflakecomputing.com");
        spec.authorization_endpoint = Some(format!("{}/oauth/authorize", spec.issuer_url));
        spec.token_endpoint = Some(format!("{}/oauth/token-request", spec.issuer_url));
        spec.token_endpoint_auth_method = Default::default();
    }
    let endpoint = if let Some(endpoint) = &spec.token_endpoint {
        endpoint.clone()
    } else {
        crate::server::extensions::providers::oauth::endpoints::resolve_oauth_endpoints(
            &spec,
            &settings.server.ssrf,
        )
        .await
        .map_err(anyhow::Error::msg)?
        .token_endpoint
    };
    oauth_client_secret_context(ext.project_id, &ext.extension, &spec, &endpoint)
}
