use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_ecr::Client as EcrClient;
use aws_sdk_sts::Client as StsClient;
use base64::Engine;
use std::sync::RwLock;
use std::time::Instant;

use crate::server::registry::{
    models::{EcrConfig, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// AWS ECR registry provider with scoped credentials via STS AssumeRole
pub struct EcrProvider {
    config: EcrConfig,
    sts_client: StsClient,
    /// Default registry path with prefix (e.g., "459109751375.dkr.ecr.eu-west-1.amazonaws.com/rise/")
    /// Used when no specific repository is provided to get_ecr_auth_token()
    registry_url: String,
    /// Registry host without path (e.g., "459109751375.dkr.ecr.eu-west-1.amazonaws.com")
    registry_host: String,
    /// Cache for pull credentials (ECR tokens valid for 12 hours)
    cached_pull_creds: RwLock<Option<(String, String, Instant)>>,
    /// `config.strict_external_hosts` pre-canonicalized (lowercased + port
    /// stripped) so the trait default validator can byte-compare without
    /// allocating per call.
    normalized_strict_external_hosts: Vec<String>,
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

        // Build registry_host: {account}.dkr.ecr.{region}.amazonaws.com
        let registry_host = format!(
            "{}.dkr.ecr.{}.amazonaws.com",
            config.account_id, config.region
        );

        // Build registry_url: {registry_host}/{repo_prefix}
        // repo_prefix is literal (e.g., "rise/" → "rise/hello")
        let registry_url = format!("{}/{}", registry_host, config.repo_prefix);

        let normalized_strict_external_hosts = config
            .strict_external_hosts
            .iter()
            .map(|h| crate::server::registry::normalize_host(h))
            .collect();

        Ok(Self {
            config,
            sts_client,
            registry_url,
            registry_host,
            cached_pull_creds: RwLock::new(None),
            normalized_strict_external_hosts,
        })
    }

    /// Cache TTL for pull credentials (11 hours, 1 hour buffer before 12h expiry)
    const CACHE_TTL_SECS: u64 = 11 * 60 * 60;

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

#[async_trait]
impl RegistryProvider for EcrProvider {
    async fn get_credentials(&self, repository: &str, _tag: &str) -> Result<RegistryCredentials> {
        tracing::info!(
            "Getting scoped ECR credentials for repository: {}",
            repository
        );

        // Full ECR repository name: {repo_prefix}{project}
        // repo_prefix is literal, e.g., "rise/" + "hello" = "rise/hello"
        let repo_name = format!("{}{}", self.config.repo_prefix, repository);

        // Generate scoped credentials via AssumeRole with inline session policy
        let repo_arn = format!(
            "arn:aws:ecr:{}:{}:repository/{}",
            self.config.region, self.config.account_id, repo_name
        );

        tracing::debug!("ECR repository ARN for policy: {}", repo_arn);

        let inline_policy = serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": [
                    "ecr:GetAuthorizationToken"
                ],
                "Resource": "*"
            }, {
                "Effect": "Allow",
                "Action": [
                    "ecr:BatchCheckLayerAvailability",
                    "ecr:InitiateLayerUpload",
                    "ecr:UploadLayerPart",
                    "ecr:CompleteLayerUpload",
                    "ecr:PutImage",
                    "ecr:BatchGetImage",
                    "ecr:GetDownloadUrlForLayer"
                ],
                "Resource": repo_arn
            }]
        });

        tracing::debug!(
            "Assuming push role {} with scoped policy for repository {} with inline policy: {}",
            self.config.push_role_arn,
            repo_name,
            inline_policy.to_string()
        );

        let assumed_role = self
            .sts_client
            .assume_role()
            .role_arn(&self.config.push_role_arn)
            .role_session_name(format!("rise-push-{}", repository))
            .policy(inline_policy.to_string())
            .send()
            .await
            .context("Failed to assume ECR push role")?;

        let creds = assumed_role
            .credentials()
            .context("No credentials in AssumeRole response")?;

        // Create ECR client with scoped credentials
        // Convert AWS DateTime to SystemTime
        let expiration: Option<std::time::SystemTime> =
            std::time::SystemTime::try_from(*creds.expiration()).ok();

        let scoped_creds = aws_sdk_ecr::config::Credentials::new(
            creds.access_key_id(),
            creds.secret_access_key(),
            Some(creds.session_token().to_string()),
            expiration,
            "assume_role",
        );

        let scoped_aws_config = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(scoped_creds)
            .region(aws_config::Region::new(self.config.region.clone()))
            .load()
            .await;

        let scoped_ecr_client = EcrClient::new(&scoped_aws_config);

        // Get ECR auth token with scoped credentials for this specific repository
        self.get_ecr_auth_token(&scoped_ecr_client, &repo_name)
            .await
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        // Check cache first
        {
            let cache = self.cached_pull_creds.read().unwrap();
            if let Some((user, pass, created)) = cache.as_ref() {
                if created.elapsed().as_secs() < Self::CACHE_TTL_SECS {
                    tracing::debug!("Using cached ECR pull credentials");
                    return Ok((user.clone(), pass.clone()));
                }
            }
        }

        tracing::info!("Fetching fresh ECR pull credentials via push role");

        // Assume the push role (which has BatchGetImage permission)
        let assumed_role = self
            .sts_client
            .assume_role()
            .role_arn(&self.config.push_role_arn)
            .role_session_name("rise-pull-credentials")
            .send()
            .await
            .context("Failed to assume ECR push role for pull credentials")?;

        let creds = assumed_role
            .credentials()
            .context("No credentials in AssumeRole response")?;

        // Create ECR client with assumed role credentials
        let expiration: Option<std::time::SystemTime> =
            std::time::SystemTime::try_from(*creds.expiration()).ok();

        let assumed_creds = aws_sdk_ecr::config::Credentials::new(
            creds.access_key_id(),
            creds.secret_access_key(),
            Some(creds.session_token().to_string()),
            expiration,
            "assume_role",
        );

        let assumed_aws_config = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(assumed_creds)
            .region(aws_config::Region::new(self.config.region.clone()))
            .load()
            .await;

        let ecr_client = EcrClient::new(&assumed_aws_config);
        let (username, password) = self.decode_ecr_token(&ecr_client).await?;

        // Update cache
        {
            let mut cache = self.cached_pull_creds.write().unwrap();
            *cache = Some((username.clone(), password.clone(), Instant::now()));
        }

        Ok((username, password))
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

    fn strict_external_hosts(&self) -> &[String] {
        &self.normalized_strict_external_hosts
    }
}
