use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_ecr::Client as EcrClient;
use uuid::Uuid;

use crate::server::registry::models::EcrConfig;

/// Extract a clean error message from an AWS SDK error's Debug output
///
/// The AWS SDK errors have verbose Debug output, but we can extract just the
/// meaningful message by parsing for the `message: Some("...")` pattern.
fn format_sdk_error<E: std::fmt::Debug>(err: &E) -> String {
    let debug_str = format!("{:?}", err);

    // Try to extract the message field from the debug output
    // Pattern: message: Some("actual error message")
    if let Some(start) = debug_str.find("message: Some(\"") {
        let start = start + 15; // length of 'message: Some("'
        if let Some(end) = debug_str[start..].find("\")") {
            return debug_str[start..start + end].to_string();
        }
    }

    // Fallback: try to find just a Message field (as in JSON response)
    if let Some(start) = debug_str.find("\"Message\":\"") {
        let start = start + 11; // length of '"Message":"'
        if let Some(end) = debug_str[start..].find("\"") {
            return debug_str[start..start + end].to_string();
        }
    }

    // Last resort: return a truncated debug string
    if debug_str.len() > 200 {
        format!("{}...", &debug_str[..200])
    } else {
        debug_str
    }
}

/// The tag `tag_as_orphaned` stamps on a deleted project's repository.
pub const ORPHANED_TAG_KEY: &str = "rise:orphaned";
pub const CREDENTIAL_GENERATION_TAG_KEY: &str = "rise:credential-generation";
pub const PROJECT_ID_TAG_KEY: &str = "rise:project-id";

pub struct RepositoryAccess {
    pub orphaned: bool,
    pub credential_generation: Option<String>,
    pub project_id: Option<String>,
}

impl RepositoryAccess {
    fn ensure_owner(&self, project_id: Uuid, allow_unbound: bool) -> Result<()> {
        match &self.project_id {
            Some(owner) => anyhow::ensure!(
                *owner == project_id.to_string(),
                "ECR repository belongs to a different project instance; an operator must review ownership before assigning it"
            ),
            None => anyhow::ensure!(
                allow_unbound,
                "ECR repository has no project ownership binding; an operator must verify ownership before assigning it"
            ),
        }
        Ok(())
    }
}

fn credential_generation_tag() -> Result<aws_sdk_ecr::types::Tag> {
    aws_sdk_ecr::types::Tag::builder()
        .key(CREDENTIAL_GENERATION_TAG_KEY)
        .value(uuid::Uuid::new_v4().to_string())
        .build()
        .context("Failed to build credential generation tag")
}

fn access_from_tags(tags: &[aws_sdk_ecr::types::Tag]) -> RepositoryAccess {
    RepositoryAccess {
        orphaned: tags_mark_orphaned(tags),
        credential_generation: tags
            .iter()
            .find(|tag| tag.key() == CREDENTIAL_GENERATION_TAG_KEY)
            .map(|tag| tag.value().to_string())
            .filter(|value| !value.is_empty()),
        project_id: tags
            .iter()
            .find(|tag| tag.key() == PROJECT_ID_TAG_KEY)
            .map(|tag| tag.value().to_string()),
    }
}

fn project_access_tags(
    access: &RepositoryAccess,
    project_id: Uuid,
    allow_unbound: bool,
) -> Result<Vec<aws_sdk_ecr::types::Tag>> {
    anyhow::ensure!(!access.orphaned, "Orphaned repositories cannot be adopted");
    access.ensure_owner(project_id, allow_unbound)?;
    let mut tags = Vec::new();
    if access.project_id.is_none() {
        tags.push(
            aws_sdk_ecr::types::Tag::builder()
                .key(PROJECT_ID_TAG_KEY)
                .value(project_id.to_string())
                .build()
                .context("Failed to build project ownership tag")?,
        );
    }
    if access.project_id.is_none() || access.credential_generation.is_none() {
        tags.push(credential_generation_tag()?);
    }
    Ok(tags)
}

/// Whether `tags` mark a repository as left behind by a deleted project.
pub fn tags_mark_orphaned(tags: &[aws_sdk_ecr::types::Tag]) -> bool {
    tags.iter()
        .any(|tag| tag.key() == ORPHANED_TAG_KEY && tag.value() == "true")
}

pub async fn repository_access(
    client: &EcrClient,
    repo_name: &str,
) -> Result<Option<RepositoryAccess>> {
    let repositories = match client
        .describe_repositories()
        .repository_names(repo_name)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            if err
                .as_service_error()
                .is_some_and(|e| e.is_repository_not_found_exception())
            {
                return Ok(None);
            }
            return Err(anyhow::anyhow!(
                "Failed to describe ECR repository '{}': {}",
                repo_name,
                format_sdk_error(&err)
            ));
        }
    };
    let Some(arn) = repositories
        .repositories()
        .first()
        .and_then(|repository| repository.repository_arn())
    else {
        return Ok(None);
    };
    let tags = client
        .list_tags_for_resource()
        .resource_arn(arn)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to list tags of ECR repository '{}': {}",
                repo_name,
                format_sdk_error(&e)
            )
        })?;
    Ok(Some(access_from_tags(tags.tags())))
}

/// Manages ECR repository lifecycle operations
///
/// This manager handles creating, deleting, and tagging ECR repositories.
/// The controller identity manages repositories within the configured prefix.
pub struct EcrRepoManager {
    config: EcrConfig,
    ecr_client: EcrClient,
}

impl EcrRepoManager {
    /// Create a new ECR repository manager
    pub async fn new(config: EcrConfig) -> Result<Self> {
        // Build AWS config
        let aws_config = if let (Some(access_key), Some(secret_key)) =
            (&config.access_key_id, &config.secret_access_key)
        {
            // Use static credentials if provided
            let creds =
                aws_sdk_ecr::config::Credentials::new(access_key, secret_key, None, None, "static");
            aws_config::defaults(BehaviorVersion::latest())
                .credentials_provider(creds)
                .region(aws_config::Region::new(config.region.clone()))
                .load()
                .await
        } else {
            // Use default credential chain (IAM role, env vars, etc.)
            aws_config::defaults(BehaviorVersion::latest())
                .region(aws_config::Region::new(config.region.clone()))
                .load()
                .await
        };

        let ecr_client = EcrClient::new(&aws_config);

        Ok(Self { config, ecr_client })
    }

    /// Get the full ECR repository name for a project
    ///
    /// # Example
    /// With repo_prefix = "rise/" and project = "hello":
    /// Returns "rise/hello"
    pub fn repo_name(&self, project: &str) -> String {
        format!("{}{}", self.config.repo_prefix, project)
    }

    /// Get the full ECR repository ARN for a project
    pub fn repo_arn(&self, project: &str) -> String {
        format!(
            "arn:aws:ecr:{}:{}:repository/{}",
            self.config.region,
            self.config.account_id,
            self.repo_name(project)
        )
    }

    /// Check if an ECR repository exists for a project
    pub async fn repository_exists(&self, project: &str) -> Result<bool> {
        let repo_name = self.repo_name(project);

        match self
            .ecr_client
            .describe_repositories()
            .repository_names(&repo_name)
            .send()
            .await
        {
            Ok(response) => Ok(!response.repositories().is_empty()),
            Err(err) => {
                // Check if error is RepositoryNotFoundException
                if let Some(service_err) = err.as_service_error() {
                    if service_err.is_repository_not_found_exception() {
                        return Ok(false);
                    }
                }
                Err(anyhow::anyhow!(
                    "Failed to check ECR repository existence for '{}': {}",
                    repo_name,
                    format_sdk_error(&err)
                ))
            }
        }
    }

    /// Create an ECR repository for a project
    ///
    /// Creates the repository with tags for management:
    /// - rise:managed = "true"
    /// - rise:project = "{project_name}"
    /// - rise:project-id = "{project_uuid}"
    ///
    /// Returns true if created, false if already exists.
    pub async fn create_repository(&self, project: &str, project_id: Uuid) -> Result<bool> {
        let repo_name = self.repo_name(project);

        // Check if already exists
        if self.repository_exists(project).await? {
            self.ensure_project_access(project, project_id, false)
                .await?;
            tracing::debug!("ECR repository {} already exists", repo_name);
            return Ok(false);
        }

        tracing::info!("Creating ECR repository: {}", repo_name);

        // Build tags
        let managed_tag = aws_sdk_ecr::types::Tag::builder()
            .key("rise:managed")
            .value("true")
            .build()
            .context("Failed to build managed tag")?;

        let project_tag = aws_sdk_ecr::types::Tag::builder()
            .key("rise:project")
            .value(project)
            .build()
            .context("Failed to build project tag")?;

        // Create the repository with tags
        self.ecr_client
            .create_repository()
            .repository_name(&repo_name)
            .tags(managed_tag)
            .tags(project_tag)
            .tags(
                aws_sdk_ecr::types::Tag::builder()
                    .key(PROJECT_ID_TAG_KEY)
                    .value(project_id.to_string())
                    .build()
                    .context("Failed to build project ownership tag")?,
            )
            .tags(credential_generation_tag()?)
            .image_scanning_configuration(
                aws_sdk_ecr::types::ImageScanningConfiguration::builder()
                    .scan_on_push(true)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to create ECR repository '{}': {}",
                    repo_name,
                    format_sdk_error(&e)
                )
            })?;

        tracing::info!("Created ECR repository: {}", repo_name);
        Ok(true)
    }

    /// Active repositories bind credentials to their project and generation.
    pub async fn ensure_project_access(
        &self,
        project: &str,
        project_id: Uuid,
        allow_unbound: bool,
    ) -> Result<()> {
        let access = repository_access(&self.ecr_client, &self.repo_name(project))
            .await?
            .context("Repository is not provisioned")?;
        let tags = project_access_tags(&access, project_id, allow_unbound)?;
        if !tags.is_empty() {
            self.ecr_client
                .tag_resource()
                .resource_arn(self.repo_arn(project))
                .set_tags(Some(tags))
                .send()
                .await
                .context("Failed to establish ECR project credential scope")?;
        }
        Ok(())
    }

    /// Delete an ECR repository for a project
    ///
    /// Deletes the repository and all images within it.
    /// Returns true if deleted, false if it didn't exist.
    pub async fn delete_repository(&self, project: &str, project_id: Uuid) -> Result<bool> {
        let repo_name = self.repo_name(project);

        // Check if exists
        let Some(access) = self.repository_access(project).await? else {
            tracing::debug!(
                "ECR repository {} does not exist, nothing to delete",
                repo_name
            );
            return Ok(false);
        };
        access.ensure_owner(project_id, true)?;

        tracing::info!("Deleting ECR repository: {}", repo_name);

        // Delete the repository (force = true deletes images too)
        self.ecr_client
            .delete_repository()
            .repository_name(&repo_name)
            .force(true)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to delete ECR repository '{}': {}",
                    repo_name,
                    format_sdk_error(&e)
                )
            })?;

        tracing::info!("Deleted ECR repository: {}", repo_name);
        Ok(true)
    }

    /// Tag a repository as orphaned (when soft-delete is configured)
    ///
    /// Adds the tag rise:orphaned = "true" to the repository.
    /// This marks the repository for manual cleanup instead of automatic deletion.
    ///
    /// Returns true if tagged, false if repository doesn't exist.
    pub async fn tag_as_orphaned(&self, project: &str, project_id: Uuid) -> Result<bool> {
        // Check if repository exists first
        let Some(access) = self.repository_access(project).await? else {
            tracing::debug!(
                "ECR repository {} does not exist, nothing to tag",
                self.repo_name(project)
            );
            return Ok(false);
        };
        access.ensure_owner(project_id, true)?;

        let repo_arn = self.repo_arn(project);

        tracing::info!(
            "Tagging ECR repository as orphaned: {}",
            self.repo_name(project)
        );

        let orphaned_tag = aws_sdk_ecr::types::Tag::builder()
            .key(ORPHANED_TAG_KEY)
            .value("true")
            .build()
            .context("Failed to build orphaned tag")?;

        self.ecr_client
            .tag_resource()
            .resource_arn(&repo_arn)
            .tags(orphaned_tag)
            .tags(credential_generation_tag()?)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to tag ECR repository '{}' as orphaned: {}",
                    self.repo_name(project),
                    format_sdk_error(&e)
                )
            })?;

        Ok(true)
    }

    pub async fn repository_access(&self, project: &str) -> Result<Option<RepositoryAccess>> {
        repository_access(&self.ecr_client, &self.repo_name(project)).await
    }

    /// Get whether auto_remove is enabled
    pub fn auto_remove(&self) -> bool {
        self.config.auto_remove
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ecr::types::Tag;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockRepository {
        tags: Option<Vec<serde_json::Value>>,
        writes: Vec<String>,
        fail_create_response: bool,
    }

    async fn mock_ecr(
        State(state): State<Arc<Mutex<MockRepository>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        let target = headers.get("x-amz-target").unwrap().to_str().unwrap();
        let mut state = state.lock().unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let response = if target.ends_with("DescribeRepositories") {
            if state.tags.is_none() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "__type": "RepositoryNotFoundException", "message": "not found"
                    })),
                )
                    .into_response();
            }
            serde_json::json!({"repositories": [{"repositoryArn": "arn:aws:ecr:eu-west-1:123456789012:repository/rise/app"}]})
        } else if target.ends_with("ListTagsForResource") {
            serde_json::json!({"tags": state.tags})
        } else if target.ends_with("CreateRepository") {
            state.writes.push("create".into());
            state.tags = Some(payload["tags"].as_array().unwrap().clone());
            if state.fail_create_response {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({
                        "__type": "ServerException", "message": "response failed after creation"
                    })),
                )
                    .into_response();
            }
            serde_json::json!({})
        } else if target.ends_with("TagResource") {
            state.writes.push("tag".into());
            for tag in payload["tags"].as_array().unwrap() {
                let tags = state.tags.as_mut().unwrap();
                tags.retain(|existing| existing["Key"] != tag["Key"]);
                tags.push(tag.clone());
            }
            serde_json::json!({})
        } else if target.ends_with("DeleteRepository") {
            state.writes.push("delete".into());
            state.tags = None;
            serde_json::json!({})
        } else {
            panic!("Unexpected ECR operation: {target}")
        };
        axum::Json(response).into_response()
    }

    async fn mock_manager(
        state: Arc<Mutex<MockRepository>>,
    ) -> (EcrRepoManager, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/", axum::routing::post(mock_ecr))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = aws_sdk_ecr::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .credentials_provider(aws_sdk_ecr::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .endpoint_url(endpoint)
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(1))
            .build();
        (
            EcrRepoManager {
                config: EcrConfig {
                    region: "eu-west-1".into(),
                    account_id: "123456789012".into(),
                    access_key_id: None,
                    secret_access_key: None,
                    repo_prefix: "rise/".into(),
                    push_role_arn: "unused".into(),
                    auto_remove: false,
                    registry_host: None,
                },
                ecr_client: EcrClient::from_conf(client),
            },
            server,
        )
    }

    fn tag(key: &str, value: &str) -> Tag {
        Tag::builder().key(key).value(value).build().unwrap()
    }

    #[test]
    fn only_the_orphaned_tag_marks_a_repository_orphaned() {
        assert!(tags_mark_orphaned(&[
            tag("rise:managed", "true"),
            tag(ORPHANED_TAG_KEY, "true"),
        ]));
        assert!(!tags_mark_orphaned(&[
            tag("rise:managed", "true"),
            tag("rise:project", "hello"),
        ]));
        assert!(!tags_mark_orphaned(&[tag(ORPHANED_TAG_KEY, "false")]));
        assert!(!tags_mark_orphaned(&[]));
    }

    #[test]
    fn credential_generations_are_distinct_and_preserve_orphan_state() {
        let first = credential_generation_tag().unwrap();
        let second = credential_generation_tag().unwrap();
        assert_ne!(first.value(), second.value());
        let access = access_from_tags(&[first.clone(), tag(ORPHANED_TAG_KEY, "true")]);
        assert!(access.orphaned);
        assert_eq!(access.credential_generation.as_deref(), Some(first.value()));
        assert!(access_from_tags(&[]).credential_generation.is_none());
    }

    #[test]
    fn only_verified_unbound_repositories_receive_an_owner_and_fresh_generation() {
        let owner = Uuid::new_v4();
        let legacy = access_from_tags(&[tag(CREDENTIAL_GENERATION_TAG_KEY, "previous")]);
        assert!(project_access_tags(&legacy, owner, false).is_err());
        let imported = project_access_tags(&legacy, owner, true).unwrap();
        let imported = access_from_tags(&imported);
        assert_eq!(imported.project_id, Some(owner.to_string()));
        assert_ne!(imported.credential_generation.as_deref(), Some("previous"));
        assert!(project_access_tags(&imported, owner, false)
            .unwrap()
            .is_empty());
        assert!(project_access_tags(&imported, Uuid::new_v4(), true).is_err());
        for value in ["", "invalid"] {
            let invalid = access_from_tags(&[tag(PROJECT_ID_TAG_KEY, value)]);
            assert_eq!(invalid.project_id.as_deref(), Some(value));
            assert!(project_access_tags(&invalid, owner, true).is_err());
        }
        let orphaned = access_from_tags(&[tag(ORPHANED_TAG_KEY, "true")]);
        assert!(project_access_tags(&orphaned, owner, true).is_err());
    }

    #[tokio::test]
    async fn a_failed_creation_response_cannot_give_the_repository_to_a_reused_name() {
        let state = Arc::new(Mutex::new(MockRepository {
            fail_create_response: true,
            ..Default::default()
        }));
        let (manager, server) = mock_manager(state.clone()).await;
        let owner = Uuid::new_v4();
        assert!(manager.create_repository("app", owner).await.is_err());
        let replacement = Uuid::new_v4();
        assert!(manager.create_repository("app", replacement).await.is_err());
        assert!(manager
            .ensure_project_access("app", replacement, true)
            .await
            .is_err());
        assert!(manager.delete_repository("app", replacement).await.is_err());
        assert!(manager.tag_as_orphaned("app", replacement).await.is_err());
        assert!(!manager.create_repository("app", owner).await.unwrap());
        let state = state.lock().unwrap();
        assert_eq!(state.writes, ["create"]);
        let tags = state.tags.as_ref().unwrap();
        assert!(tags
            .iter()
            .any(|tag| tag["Key"] == PROJECT_ID_TAG_KEY && tag["Value"] == owner.to_string()));
        server.abort();
    }

    #[tokio::test]
    async fn verified_import_binds_owner_and_rotates_generation_in_one_write() {
        let state = Arc::new(Mutex::new(MockRepository {
            tags: Some(vec![
                serde_json::json!({"Key": CREDENTIAL_GENERATION_TAG_KEY, "Value": "previous"}),
            ]),
            ..Default::default()
        }));
        let (manager, server) = mock_manager(state.clone()).await;
        let owner = Uuid::new_v4();
        assert!(manager.create_repository("app", owner).await.is_err());
        assert!(state.lock().unwrap().writes.is_empty());
        manager
            .ensure_project_access("app", owner, true)
            .await
            .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.writes, ["tag"]);
        let tags = state.tags.as_ref().unwrap();
        assert!(tags
            .iter()
            .any(|tag| tag["Key"] == PROJECT_ID_TAG_KEY && tag["Value"] == owner.to_string()));
        assert!(tags
            .iter()
            .any(|tag| tag["Key"] == CREDENTIAL_GENERATION_TAG_KEY && tag["Value"] != "previous"));
        server.abort();
    }
}
