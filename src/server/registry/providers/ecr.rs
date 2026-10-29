use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_ecr::Client as EcrClient;
use aws_sdk_sts::Client as StsClient;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::server::registry::{
    models::{EcrConfig, RegistryCredentials},
    ImageTagType, RegistryProvider, RepositoryUnavailable,
};

type PullCredentialCache = HashMap<(String, Uuid, String), (RegistryCredentials, Instant)>;

/// The host images are pushed to and pulled from: the configured override, or
/// the account's regional ECR endpoint.
fn registry_host(config: &EcrConfig) -> String {
    match config.registry_host.as_deref().filter(|h| !h.is_empty()) {
        Some(host) => host.to_string(),
        None => format!(
            "{}.dkr.ecr.{}.amazonaws.com",
            config.account_id, config.region
        ),
    }
}

const SCOPE_OBSERVATION_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct ScopeObservation {
    generation: Option<(String, Instant)>,
}

type ScopeCell = Arc<tokio::sync::Mutex<ScopeObservation>>;

fn observation_cache() -> moka::future::Cache<(String, Uuid), ScopeCell> {
    moka::future::Cache::builder()
        .max_capacity(10_000)
        .time_to_idle(Duration::from_secs(3600))
        .build()
}

/// AWS ECR registry provider with scoped credentials via STS AssumeRole
pub struct EcrProvider {
    config: EcrConfig,
    sts_client: StsClient,
    /// ECR client on the backend's own identity, for inspecting a repository
    /// before credentials to it are minted.
    ecr_client: EcrClient,
    /// Registry host followed by the literal repository prefix.
    registry_url: String,
    /// Registry host without path (e.g., "459109751375.dkr.ecr.eu-west-1.amazonaws.com")
    registry_host: String,
    /// Pull credentials cached by repository, project UUID and generation.
    cached_pull_creds: RwLock<PullCredentialCache>,
    /// Runtime observations expire after one minute; issuance also publishes scopes.
    observed_scopes: moka::future::Cache<(String, Uuid), ScopeCell>,
}

impl EcrProvider {
    /// Create a new ECR provider
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

        let sts_client = StsClient::new(&aws_config);
        let ecr_client = EcrClient::new(&aws_config);

        let registry_host = registry_host(&config);

        // Build registry_url: {registry_host}/{repo_prefix}
        // repo_prefix is literal (e.g., "rise/" → "rise/hello")
        let registry_url = format!("{}/{}", registry_host, config.repo_prefix);

        Ok(Self {
            config,
            sts_client,
            ecr_client,
            registry_url,
            registry_host,
            cached_pull_creds: RwLock::new(HashMap::new()),
            observed_scopes: observation_cache(),
        })
    }

    /// Cached credentials expire before the one-hour STS session.
    const CACHE_TTL_SECS: u64 = 45 * 60;

    /// Decode ECR authorization token from the client response
    ///
    /// Returns (username, password) tuple from the base64-encoded token
    async fn decode_ecr_token(&self, client: &EcrClient) -> Result<(String, String)> {
        let response = client
            .get_authorization_token()
            .send()
            .await
            .context("Failed to get ECR authorization token")?;

        let auth_data = response
            .authorization_data()
            .first()
            .context("No authorization data returned from ECR")?;

        let token = auth_data
            .authorization_token()
            .context("No authorization token in response")?;

        // Decode the base64 token (format is "AWS:password")
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(token)
            .context("Failed to decode ECR token")?;

        let decoded_str = String::from_utf8(decoded).context("ECR token is not valid UTF-8")?;

        let parts: Vec<&str> = decoded_str.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid ECR token format");
        }

        Ok((parts[0].to_string(), parts[1].to_string()))
    }

    /// Get ECR authorization token using the provided ECR client
    ///
    /// # Arguments
    /// * `client` - ECR client to use for authentication (should already be scoped with appropriate credentials)
    /// * `repo_name` - Full repository name (e.g., "rise/compass")
    async fn get_ecr_auth_token(
        &self,
        client: &EcrClient,
        repo_name: &str,
    ) -> Result<RegistryCredentials> {
        let (username, password) = self.decode_ecr_token(client).await?;

        // ECR tokens are valid for 12 hours
        let expires_in = Some(12 * 60 * 60); // 12 hours in seconds

        // Build full repository path for docker login
        // Example: "459109751375.dkr.ecr.eu-west-1.amazonaws.com/rise/compass"
        let registry_url = format!("{}/{}", self.registry_host, repo_name);

        Ok(RegistryCredentials {
            registry_url,
            username,
            password,
            expires_in,
            auth_method: Default::default(),
        })
    }
}

#[derive(Clone, Copy)]
enum RepositoryPermission {
    Pull,
    Push,
}

/// STS accepts at most 64 characters; the hash preserves repository identity.
fn role_session_name(repository: &str, permission: RepositoryPermission) -> String {
    let operation = match permission {
        RepositoryPermission::Pull => "pull",
        RepositoryPermission::Push => "push",
    };
    let hash: String = Sha256::digest(repository.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let readable: String = repository.chars().take(37).collect();
    format!("rise-{operation}-{readable}-{hash}")
}

fn repository_policy(
    repo_arn: &str,
    project_id: Uuid,
    generation: &str,
    permission: RepositoryPermission,
) -> serde_json::Value {
    let mut actions = vec![
        "ecr:BatchCheckLayerAvailability",
        "ecr:BatchGetImage",
        "ecr:GetDownloadUrlForLayer",
    ];
    if matches!(permission, RepositoryPermission::Push) {
        actions.extend([
            "ecr:InitiateLayerUpload",
            "ecr:UploadLayerPart",
            "ecr:CompleteLayerUpload",
            "ecr:PutImage",
        ]);
    }
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": ["ecr:GetAuthorizationToken"],
            "Resource": "*"
        }, {
            "Effect": "Allow",
            "Action": actions,
            "Resource": repo_arn,
            "Condition": {
                "StringEquals": {
                    "aws:ResourceTag/rise:credential-generation": generation,
                    "aws:ResourceTag/rise:project-id": project_id.to_string()
                },
                "StringNotEquals": { "aws:ResourceTag/rise:orphaned": "true" }
            }
        }]
    })
}

impl EcrProvider {
    async fn scope_observation(&self, repository: &str, project_id: Uuid) -> ScopeCell {
        // Cache initialization never performs I/O. An evicted cell cannot publish
        // itself over the current cell when its in-flight request completes.
        self.observed_scopes
            .get_with((repository.to_string(), project_id), async {
                Arc::new(tokio::sync::Mutex::new(ScopeObservation::default()))
            })
            .await
    }

    async fn repository_generation(&self, repository: &str, project_id: Uuid) -> Result<String> {
        let repo_name = format!("{}{}", self.config.repo_prefix, repository);
        let access = crate::server::ecr::repository_access(&self.ecr_client, &repo_name)
            .await?
            .ok_or_else(|| {
                anyhow::Error::new(RepositoryUnavailable(format!(
                    "Repository '{repo_name}' is not provisioned yet"
                )))
            })?;
        if access.orphaned {
            return Err(anyhow::Error::new(RepositoryUnavailable(format!(
                "Repository '{repo_name}' still holds the images of a deleted project named \
                 '{repository}'. Ask an operator to delete it before deploying this project."
            ))));
        }
        if access.project_id.as_deref() != Some(project_id.to_string().as_str()) {
            return Err(anyhow::Error::new(RepositoryUnavailable(format!(
                "Repository '{repo_name}' is not bound to this project instance. \
                 Ask an operator to review its ownership."
            ))));
        }
        access.credential_generation.ok_or_else(|| {
            anyhow::Error::new(RepositoryUnavailable(format!(
                "Repository '{repo_name}' is waiting for credential-generation provisioning"
            )))
        })
    }

    async fn cached_project_pull_credentials(
        &self,
        repository: &str,
        project_id: Uuid,
        generation: &str,
    ) -> Result<RegistryCredentials> {
        let cache_key = (repository.to_string(), project_id, generation.to_string());
        {
            let cache = self.cached_pull_creds.read().unwrap();
            if let Some((credentials, created)) = cache.get(&cache_key) {
                if created.elapsed().as_secs() < Self::CACHE_TTL_SECS {
                    let mut credentials = credentials.clone();
                    credentials.expires_in = credentials
                        .expires_in
                        .map(|seconds| seconds.saturating_sub(created.elapsed().as_secs()));
                    return Ok(credentials);
                }
            }
        }
        let credentials = self
            .scoped_credentials(
                repository,
                project_id,
                generation,
                RepositoryPermission::Pull,
            )
            .await?;
        let mut cache = self.cached_pull_creds.write().unwrap();
        cache.retain(|_, (_, created)| created.elapsed().as_secs() < Self::CACHE_TTL_SECS);
        cache.insert(cache_key, (credentials.clone(), Instant::now()));
        Ok(credentials)
    }

    async fn scoped_credentials(
        &self,
        repository: &str,
        project_id: Uuid,
        generation: &str,
        permission: RepositoryPermission,
    ) -> Result<RegistryCredentials> {
        let repo_name = format!("{}{}", self.config.repo_prefix, repository);
        let repo_arn = format!(
            "arn:aws:ecr:{}:{}:repository/{}",
            self.config.region, self.config.account_id, repo_name
        );
        let policy = repository_policy(&repo_arn, project_id, generation, permission);
        let assumed_role = self
            .sts_client
            .assume_role()
            .role_arn(&self.config.push_role_arn)
            .role_session_name(role_session_name(repository, permission))
            .duration_seconds(3600)
            .policy(policy.to_string())
            .send()
            .await
            .context("Failed to assume repository-scoped ECR role")?;
        let credentials = assumed_role
            .credentials()
            .context("No credentials in AssumeRole response")?;
        let expiration = std::time::SystemTime::try_from(*credentials.expiration()).ok();
        let credentials = aws_sdk_ecr::config::Credentials::new(
            credentials.access_key_id(),
            credentials.secret_access_key(),
            Some(credentials.session_token().to_string()),
            expiration,
            "assume_role",
        );
        let scoped_config = self
            .ecr_client
            .config()
            .to_builder()
            .credentials_provider(credentials)
            .build();
        let mut credentials = self
            .get_ecr_auth_token(&EcrClient::from_conf(scoped_config), &repo_name)
            .await?;
        credentials.expires_in = Some(3600);
        Ok(credentials)
    }
}

#[async_trait]
impl RegistryProvider for EcrProvider {
    async fn get_credentials(
        &self,
        repository: &str,
        project_id: Uuid,
        _tags: &[&str],
    ) -> Result<RegistryCredentials> {
        let cell = self.scope_observation(repository, project_id).await;
        let mut observation = cell.lock().await;
        let generation = self.repository_generation(repository, project_id).await?;
        observation.generation = Some((generation.clone(), Instant::now()));
        self.scoped_credentials(
            repository,
            project_id,
            &generation,
            RepositoryPermission::Push,
        )
        .await
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        anyhow::bail!("ECR pull credentials require a project repository")
    }

    async fn get_project_pull_credentials(
        &self,
        repository: &str,
        project_id: Uuid,
    ) -> Result<RegistryCredentials> {
        Ok(self
            .get_project_pull_credentials_with_scope(repository, project_id)
            .await?
            .0)
    }

    async fn get_project_pull_credentials_with_scope(
        &self,
        repository: &str,
        project_id: Uuid,
    ) -> Result<(RegistryCredentials, Option<String>)> {
        // Serialize authoritative reads, minting and publication per repository.
        let cell = self.scope_observation(repository, project_id).await;
        let mut observation = cell.lock().await;
        let generation = self.repository_generation(repository, project_id).await?;
        observation.generation = Some((generation.clone(), Instant::now()));
        let credentials = self
            .cached_project_pull_credentials(repository, project_id, &generation)
            .await?;
        Ok((credentials, Some(generation)))
    }

    async fn project_pull_credential_scope(
        &self,
        repository: &str,
        project_id: Uuid,
    ) -> Result<Option<String>> {
        let cell = self.scope_observation(repository, project_id).await;
        let mut observation = cell.lock().await;
        if let Some((generation, observed_at)) = &observation.generation {
            if observed_at.elapsed() < SCOPE_OBSERVATION_TTL {
                return Ok(Some(generation.clone()));
            }
        }
        let generation = self.repository_generation(repository, project_id).await?;
        observation.generation = Some((generation.clone(), Instant::now()));
        Ok(Some(generation))
    }

    async fn get_k8s_pull_credentials(
        &self,
        repository: &str,
        project_id: Uuid,
    ) -> Result<RegistryCredentials> {
        self.get_project_pull_credentials(repository, project_id)
            .await
    }

    fn image_project<'a>(&self, image: &'a str) -> Option<&'a str> {
        // ECR prefixes are literal: "rise-" names "rise-app", not "rise-/app".
        image
            .strip_prefix(&self.registry_url)?
            .split([':', '/', '@'])
            .next()
            .filter(|name| !name.is_empty())
    }

    fn registry_host(&self) -> &str {
        &self.registry_host
    }

    fn registry_url(&self) -> &str {
        &self.registry_url
    }

    fn get_image_tag(&self, repository: &str, tag: &str, _tag_type: ImageTagType) -> String {
        // ECR doesn't differentiate between client and internal - always use same path
        let repo_name = format!("{}{}", self.config.repo_prefix, repository);
        format!("{}/{}:{}", self.registry_host, repo_name, tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, extract::State, http::HeaderMap, response::IntoResponse};
    use std::sync::{Arc, Mutex};

    const PROJECT_ID: Uuid = Uuid::from_u128(1);

    #[derive(Default)]
    struct RequestBarrier {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    struct MockAccess {
        orphaned: bool,
        project_id: Option<String>,
        generation: Option<String>,
        policies: Vec<serde_json::Value>,
        session_names: Vec<String>,
        metadata_reads: usize,
        blocked_metadata: Option<Arc<RequestBarrier>>,
    }

    async fn mock_registry(
        State(state): State<Arc<Mutex<MockAccess>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        let target = headers
            .get("x-amz-target")
            .and_then(|target| target.to_str().ok())
            .unwrap_or_default();
        let (response, barrier) = {
            let mut state = state.lock().unwrap();
            let response = if target.ends_with("DescribeRepositories") {
                state.metadata_reads += 1;
                serde_json::json!({ "repositories": [{
                "repositoryArn": "arn:aws:ecr:eu-west-1:459109751375:repository/rise/app"
            }] })
            } else if target.ends_with("ListTagsForResource") {
                state.metadata_reads += 1;
                let mut tags = Vec::new();
                if state.orphaned {
                    tags.push(serde_json::json!({ "Key": "rise:orphaned", "Value": "true" }));
                }
                if let Some(project_id) = &state.project_id {
                    tags.push(serde_json::json!({
                        "Key": "rise:project-id", "Value": project_id
                    }));
                }
                if let Some(generation) = &state.generation {
                    tags.push(serde_json::json!({
                        "Key": "rise:credential-generation", "Value": generation
                    }));
                }
                serde_json::json!({ "tags": tags })
            } else {
                let fields: HashMap<_, _> =
                    url::form_urlencoded::parse(&body).into_owned().collect();
                if let Some(name) = fields.get("RoleSessionName") {
                    state.session_names.push(name.clone());
                }
                if let Some(policy) = fields.get("Policy") {
                    state.policies.push(serde_json::from_str(policy).unwrap());
                }
                return (
                axum::http::StatusCode::FORBIDDEN,
                [("content-type", "text/xml")],
                "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>test</Message></Error><RequestId>test</RequestId></ErrorResponse>",
            )
                .into_response();
            };
            let barrier = if target.ends_with("ListTagsForResource") {
                state.blocked_metadata.take()
            } else {
                None
            };
            (response, barrier)
        };
        if let Some(barrier) = barrier {
            barrier.started.notify_one();
            barrier.release.notified().await;
        }
        axum::Json(response).into_response()
    }

    async fn mock_provider(
        state: Arc<Mutex<MockAccess>>,
    ) -> (EcrProvider, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/", axum::routing::post(mock_registry))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let credentials = aws_sdk_ecr::config::Credentials::new("test", "test", None, None, "test");
        let ecr = aws_sdk_ecr::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .credentials_provider(credentials.clone())
            .endpoint_url(&endpoint)
            .build();
        let sts = aws_sdk_sts::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_config::Region::new("eu-west-1"))
            .credentials_provider(credentials)
            .endpoint_url(&endpoint)
            .build();
        let config = config(None);
        let registry_host = registry_host(&config);
        (
            EcrProvider {
                registry_url: format!("{registry_host}/rise/"),
                registry_host,
                config,
                sts_client: StsClient::from_conf(sts),
                ecr_client: EcrClient::from_conf(ecr),
                cached_pull_creds: RwLock::new(HashMap::new()),
                observed_scopes: observation_cache(),
            },
            server,
        )
    }

    fn config(registry_host: Option<&str>) -> EcrConfig {
        EcrConfig {
            region: "eu-west-1".into(),
            account_id: "459109751375".into(),
            access_key_id: None,
            secret_access_key: None,
            repo_prefix: "rise/".into(),
            push_role_arn: "arn:aws:iam::459109751375:role/rise-ecr-push".into(),
            auto_remove: false,
            registry_host: registry_host.map(str::to_string),
        }
    }

    #[test]
    fn the_host_defaults_to_the_regional_ecr_endpoint() {
        assert_eq!(
            registry_host(&config(None)),
            "459109751375.dkr.ecr.eu-west-1.amazonaws.com"
        );
        // The ECS config interpolates an unset env var to an empty string.
        assert_eq!(
            registry_host(&config(Some(""))),
            "459109751375.dkr.ecr.eu-west-1.amazonaws.com"
        );
    }

    #[test]
    fn a_configured_host_replaces_the_endpoint() {
        assert_eq!(
            registry_host(&config(Some(
                "459109751375.dkr.ecr.eu-west-1.localhost:4566"
            ))),
            "459109751375.dkr.ecr.eu-west-1.localhost:4566"
        );
    }

    #[tokio::test]
    async fn pull_cache_cannot_bypass_orphan_or_generation_checks() {
        let state = Arc::new(Mutex::new(MockAccess {
            orphaned: false,
            project_id: Some(PROJECT_ID.to_string()),
            generation: Some("first".into()),
            policies: vec![],
            session_names: vec![],
            metadata_reads: 0,
            blocked_metadata: None,
        }));
        let (provider, server) = mock_provider(state.clone()).await;
        let cached = RegistryCredentials {
            registry_url: "registry/rise/app".into(),
            username: "AWS".into(),
            password: "cached".into(),
            expires_in: Some(3600),
            auth_method: Default::default(),
        };
        provider.cached_pull_creds.write().unwrap().insert(
            ("app".into(), PROJECT_ID, "first".into()),
            (cached, Instant::now()),
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            assert_eq!(
                provider
                    .get_project_pull_credentials("app", PROJECT_ID)
                    .await
                    .unwrap()
                    .password,
                "cached"
            );
            state.lock().unwrap().orphaned = true;
            let error = provider
                .get_project_pull_credentials("app", PROJECT_ID)
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<RepositoryUnavailable>().is_some());
            state.lock().unwrap().orphaned = false;
            state.lock().unwrap().generation = None;
            let error = provider
                .get_project_pull_credentials("app", PROJECT_ID)
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<RepositoryUnavailable>().is_some());
            assert!(state.lock().unwrap().policies.is_empty());
            state.lock().unwrap().generation = Some("second".into());
            assert!(provider
                .get_project_pull_credentials("app", PROJECT_ID)
                .await
                .is_err());
            {
                let state = state.lock().unwrap();
                assert_eq!(state.policies.len(), 1);
                assert_eq!(
                    state.policies[0]["Statement"][1]["Condition"]["StringEquals"]
                        ["aws:ResourceTag/rise:credential-generation"],
                    "second"
                );
            }
            assert!(provider.get_pull_credentials().await.is_err());
        })
        .await;
        server.abort();
        result.unwrap();
    }

    #[tokio::test]
    async fn ecr_literal_prefixes_extract_projects_and_bound_session_names() {
        let state = Arc::new(Mutex::new(MockAccess {
            orphaned: false,
            project_id: Some(PROJECT_ID.to_string()),
            generation: Some("generation".into()),
            policies: vec![],
            session_names: vec![],
            metadata_reads: 0,
            blocked_metadata: None,
        }));
        let (mut provider, server) = mock_provider(state.clone()).await;
        for prefix in ["rise/", "rise-", "", "nested/rise/"] {
            provider.config.repo_prefix = prefix.into();
            provider.registry_url = format!("{}/{prefix}", provider.registry_host);
            for suffix in [":tag", "@sha256:abcd"] {
                let image = format!("{}/{}app{suffix}", provider.registry_host, prefix);
                assert_eq!(provider.image_project(&image), Some("app"));
                assert_eq!(
                    provider.image_project(&format!("other/{prefix}app{suffix}")),
                    None
                );
            }
            assert_eq!(provider.image_project(&provider.registry_url), None);
        }
        let project = "a".repeat(63);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for permission in [RepositoryPermission::Pull, RepositoryPermission::Push] {
                assert!(provider
                    .scoped_credentials(&project, PROJECT_ID, "generation", permission)
                    .await
                    .is_err());
            }
        })
        .await
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.session_names.len(), 2);
        for name in &state.session_names {
            assert!(name.len() <= 64, "{name}");
        }
        assert_ne!(
            role_session_name(&project, RepositoryPermission::Pull),
            role_session_name(&format!("{}b", "a".repeat(62)), RepositoryPermission::Pull)
        );
        server.abort();
    }

    #[tokio::test]
    async fn ecr_observation_scope_cache_coalesces_expires_and_tracks_issuance() {
        let state = Arc::new(Mutex::new(MockAccess {
            orphaned: false,
            project_id: Some(PROJECT_ID.to_string()),
            generation: Some("first".into()),
            policies: vec![],
            session_names: vec![],
            metadata_reads: 0,
            blocked_metadata: None,
        }));
        let (provider, server) = mock_provider(state.clone()).await;
        assert_eq!(SCOPE_OBSERVATION_TTL, Duration::from_secs(60));
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (a, b, c) = tokio::join!(
                provider.project_pull_credential_scope("app", PROJECT_ID),
                provider.project_pull_credential_scope("app", PROJECT_ID),
                provider.project_pull_credential_scope("app", PROJECT_ID)
            );
            for scope in [a, b, c] {
                assert_eq!(scope.unwrap().as_deref(), Some("first"));
            }
            assert_eq!(state.lock().unwrap().metadata_reads, 2);
            state.lock().unwrap().generation = Some("second".into());
            assert_eq!(
                provider
                    .project_pull_credential_scope("app", PROJECT_ID)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("first")
            );
            provider
                .scope_observation("app", PROJECT_ID)
                .await
                .lock()
                .await
                .generation
                .as_mut()
                .unwrap()
                .1 = Instant::now() - Duration::from_secs(61);
            assert_eq!(
                provider
                    .project_pull_credential_scope("app", PROJECT_ID)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("second")
            );
            assert_eq!(state.lock().unwrap().metadata_reads, 4);
            state.lock().unwrap().generation = Some("third".into());
            provider.cached_pull_creds.write().unwrap().insert(
                ("app".into(), PROJECT_ID, "third".into()),
                (
                    RegistryCredentials {
                        registry_url: "registry/rise/app".into(),
                        username: "AWS".into(),
                        password: "third-token".into(),
                        expires_in: Some(3600),
                        auth_method: Default::default(),
                    },
                    Instant::now(),
                ),
            );
            let (credentials, scope) = provider
                .get_project_pull_credentials_with_scope("app", PROJECT_ID)
                .await
                .unwrap();
            assert_eq!(scope.as_deref(), Some("third"));
            assert_eq!(credentials.password, "third-token");
            assert_eq!(
                provider
                    .project_pull_credential_scope("app", PROJECT_ID)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("third")
            );
            assert_eq!(state.lock().unwrap().metadata_reads, 6);
            assert!(state.lock().unwrap().session_names.is_empty());
        })
        .await;
        server.abort();
        result.unwrap();
    }

    fn cache_test_token(provider: &EcrProvider, generation: &str) {
        provider.cached_pull_creds.write().unwrap().insert(
            ("app".into(), PROJECT_ID, generation.into()),
            (
                RegistryCredentials {
                    registry_url: "registry/rise/app".into(),
                    username: "AWS".into(),
                    password: generation.into(),
                    expires_in: Some(3600),
                    auth_method: Default::default(),
                },
                Instant::now(),
            ),
        );
    }

    #[tokio::test]
    async fn project_name_reuse_cannot_reuse_credentials_or_cached_scope() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let state = Arc::new(Mutex::new(MockAccess {
                orphaned: false,
                project_id: Some(PROJECT_ID.to_string()),
                generation: Some("first".into()),
                policies: vec![],
                session_names: vec![],
                metadata_reads: 0,
                blocked_metadata: None,
            }));
            let (provider, server) = mock_provider(state.clone()).await;
            cache_test_token(&provider, "first");
            assert_eq!(
                provider
                    .get_project_pull_credentials("app", PROJECT_ID)
                    .await
                    .unwrap()
                    .password,
                "first"
            );
            let new_project_id = Uuid::from_u128(2);
            for owner in [Some(PROJECT_ID.to_string()), None, Some(String::new())] {
                state.lock().unwrap().project_id = owner;
                for error in [
                    provider
                        .get_credentials("app", new_project_id, &["tag"])
                        .await
                        .unwrap_err(),
                    provider
                        .get_project_pull_credentials("app", new_project_id)
                        .await
                        .unwrap_err(),
                    provider
                        .project_pull_credential_scope("app", new_project_id)
                        .await
                        .unwrap_err(),
                    provider
                        .get_k8s_pull_credentials("app", new_project_id)
                        .await
                        .unwrap_err(),
                ] {
                    assert!(error.downcast_ref::<RepositoryUnavailable>().is_some());
                }
            }
            assert!(state.lock().unwrap().policies.is_empty());

            state.lock().unwrap().project_id = Some(new_project_id.to_string());
            // Even if an operator preserves the generation, a different owner's
            // token must not be returned from the credential cache.
            assert!(provider
                .get_project_pull_credentials("app", new_project_id)
                .await
                .is_err());
            let state = state.lock().unwrap();
            assert_eq!(state.policies.len(), 1);
            assert_eq!(
                state.policies[0]["Statement"][1]["Condition"]["StringEquals"]
                    ["aws:ResourceTag/rise:project-id"],
                new_project_id.to_string()
            );
            server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn scope_observation_and_issuance_are_ordered_without_stale_cell_resurrection() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for old_request_is_issuance in [false, true] {
                for evict_cell in [false, true] {
                    let barrier = Arc::new(RequestBarrier::default());
                    let state = Arc::new(Mutex::new(MockAccess {
                        orphaned: false,
                        project_id: Some(PROJECT_ID.to_string()),
                        generation: Some("first".into()),
                        policies: vec![],
                        session_names: vec![],
                        metadata_reads: 0,
                        blocked_metadata: Some(barrier.clone()),
                    }));
                    let (provider, server) = mock_provider(state.clone()).await;
                    cache_test_token(&provider, "first");
                    cache_test_token(&provider, "second");
                    let provider = Arc::new(provider);
                    let old_provider = provider.clone();
                    let old_request = tokio::spawn(async move {
                        if old_request_is_issuance {
                            old_provider
                                .get_project_pull_credentials_with_scope("app", PROJECT_ID)
                                .await
                                .unwrap()
                                .1
                        } else {
                            old_provider
                                .project_pull_credential_scope("app", PROJECT_ID)
                                .await
                                .unwrap()
                        }
                    });
                    barrier.started.notified().await;
                    state.lock().unwrap().generation = Some("second".into());
                    if evict_cell {
                        provider
                            .observed_scopes
                            .invalidate(&("app".into(), PROJECT_ID))
                            .await;
                    }
                    let new_provider = provider.clone();
                    let mut issued = tokio::spawn(async move {
                        new_provider
                            .get_project_pull_credentials_with_scope("app", PROJECT_ID)
                            .await
                            .unwrap()
                    });
                    // A blocked repository never holds up a different repository.
                    assert_eq!(
                        provider
                            .project_pull_credential_scope("other", PROJECT_ID)
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("second")
                    );
                    if evict_cell {
                        // The current cell can complete while an evicted cell is blocked.
                        let (credentials, scope) = (&mut issued).await.unwrap();
                        assert_eq!(scope.as_deref(), Some("second"));
                        assert_eq!(credentials.password, "second");
                        barrier.release.notify_one();
                        assert_eq!(old_request.await.unwrap().as_deref(), Some("first"));
                    } else {
                        assert!(tokio::time::timeout(Duration::from_millis(30), &mut issued)
                            .await
                            .is_err());
                        barrier.release.notify_one();
                        assert_eq!(old_request.await.unwrap().as_deref(), Some("first"));
                        let (credentials, scope) = issued.await.unwrap();
                        assert_eq!(scope.as_deref(), Some("second"));
                        assert_eq!(credentials.password, "second");
                    }
                    assert_eq!(
                        provider
                            .project_pull_credential_scope("app", PROJECT_ID)
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("second")
                    );
                    server.abort();
                }
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn repository_policies_bind_generation_and_limit_pull_actions() {
        let arn = "arn:aws:ecr:eu-west-1:459109751375:repository/rise/app";
        let first = repository_policy(arn, PROJECT_ID, "first", RepositoryPermission::Pull);
        let second = repository_policy(arn, PROJECT_ID, "second", RepositoryPermission::Pull);
        assert_ne!(first, second);
        assert_ne!(
            first,
            repository_policy(arn, Uuid::from_u128(2), "first", RepositoryPermission::Pull)
        );
        let repository = &first["Statement"][1];
        assert_eq!(repository["Resource"], arn);
        assert_eq!(
            repository["Condition"]["StringEquals"]["aws:ResourceTag/rise:project-id"],
            PROJECT_ID.to_string()
        );
        assert_eq!(
            repository["Condition"]["StringNotEquals"]["aws:ResourceTag/rise:orphaned"],
            "true"
        );
        let actions = repository["Action"].as_array().unwrap();
        assert!(actions.contains(&serde_json::json!("ecr:BatchGetImage")));
        assert!(actions.contains(&serde_json::json!("ecr:GetDownloadUrlForLayer")));
        assert!(!actions
            .iter()
            .any(|action| action == "ecr:PutImage" || action == "ecr:TagResource"));
        let push = repository_policy(arn, PROJECT_ID, "first", RepositoryPermission::Push);
        assert!(push["Statement"][1]["Action"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("ecr:PutImage")));
    }
}
