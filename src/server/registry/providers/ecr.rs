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

        Ok(Self {
            config,
            sts_client,
            registry_url,
            registry_host,
            cached_pull_creds: RwLock::new(None),
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

    /// Cross-project image validation with optional strict policy for known
    /// external hosts.
    ///
    /// - Images under this provider's `registry_url` prefix get the default
    ///   policy (first path segment after the prefix must equal `project_name`).
    /// - Images on a host listed in `strict_external_hosts` get a stricter
    ///   policy: the image-name path segment (last segment before `:` or `@`)
    ///   must equal `project_name`. Used for source repos that push to a
    ///   shared external registry (e.g. JFrog) where the default "external
    ///   allowed unchecked" rule would let project `evil` claim project
    ///   `compass`'s image ref.
    /// - Other external images are allowed (third-party `nginx:latest` etc.)
    ///   to keep the existing pre-built-image workflow working.
    fn validate_image_for_project(&self, image_ref: &str, project_name: &str) -> Result<()> {
        // Default-impl path: images under our own registry_url.
        let prefix = format!("{}/", self.registry_url);
        if let Some(rest) = image_ref.strip_prefix(&prefix) {
            // First path segment, stripped of any tag/digest suffix.
            let first_segment_raw = rest.split('/').next().unwrap_or("");
            let first_segment = first_segment_raw
                .split('@')
                .next()
                .unwrap_or(first_segment_raw);
            let first_segment = first_segment
                .rsplit_once(':')
                .map_or(first_segment, |(p, _)| p);
            if first_segment != project_name {
                anyhow::bail!(
                    "Image '{image_ref}' belongs to project '{first_segment}', \
                     not '{project_name}'. Cross-project image deploys are not allowed."
                );
            }
            return Ok(());
        }

        // Strict-external-host path: image is not on our registry, but is on
        // a host the operator considers "ours" (e.g. the JFrog host source
        // repos push to). Enforce the last-segment-equals-project rule.
        //
        // Hosts are normalized (lowercase, default ports stripped) on both
        // sides — the caller-digest path lets a project member supply the
        // entire `image` string, so an attacker could otherwise dodge the
        // strict policy via `JFROG.helsing-dev.ai/...` or
        // `jfrog.helsing-dev.ai:443/...`.
        if !self.config.strict_external_hosts.is_empty() {
            let raw_host = image_ref.split('/').next().unwrap_or("");
            let image_host = normalize_host(raw_host);
            if self
                .config
                .strict_external_hosts
                .iter()
                .any(|h| normalize_host(h) == image_host)
            {
                // Strip the host before tag/digest parsing so a ported,
                // untagged ref like `jfrog.example.com:443/repo/app` isn't
                // mis-parsed (the host's `:443` would otherwise be treated
                // as a tag separator).
                let after_host = image_ref.split_once('/').map_or("", |(_, rest)| rest);
                let without_digest = after_host.split('@').next().unwrap_or(after_host);
                let without_tag = without_digest
                    .rsplit_once(':')
                    .map_or(without_digest, |(p, _)| p);
                let image_name = without_tag.rsplit('/').next().unwrap_or("");
                if image_name != project_name {
                    anyhow::bail!(
                        "Image '{image_ref}' on host '{raw_host}' is owned by a different \
                         project (image name '{image_name}' != project '{project_name}'). \
                         Cross-project image deploys are not allowed for this host."
                    );
                }
                return Ok(());
            }
        }

        // Fully external image (e.g. nginx:latest) — allowed.
        Ok(())
    }
}

/// Normalize a registry host for comparison: lowercase the hostname and
/// strip the default HTTPS/HTTP port. Used so `JFROG.example.com`,
/// `jfrog.example.com`, and `jfrog.example.com:443` all compare equal
/// against entries in `strict_external_hosts`.
fn normalize_host(host: &str) -> String {
    let lower = host.to_ascii_lowercase();
    if let Some(rest) = lower.strip_suffix(":443") {
        rest.to_string()
    } else if let Some(rest) = lower.strip_suffix(":80") {
        rest.to_string()
    } else {
        lower
    }
}

#[cfg(test)]
mod validate_image_tests {
    use super::*;

    fn cfg_with_strict_hosts(strict: Vec<String>) -> EcrConfig {
        EcrConfig {
            region: "eu-west-1".to_string(),
            account_id: "123456789012".to_string(),
            access_key_id: None,
            secret_access_key: None,
            repo_prefix: "rise/".to_string(),
            push_role_arn: "arn:aws:iam::123456789012:role/rise-push".to_string(),
            auto_remove: false,
            strict_external_hosts: strict,
        }
    }

    /// Construct a provider without going through `EcrProvider::new` — that
    /// would require an actual AWS context. We only exercise the validator,
    /// which doesn't touch AWS at all.
    fn provider(strict: Vec<String>) -> EcrProvider {
        let config = cfg_with_strict_hosts(strict);
        // The validator never calls `sts_client`; we hand-build a minimal
        // config-only shell to avoid pulling in AWS context for these
        // pure-logic tests.
        let registry_host = format!(
            "{}.dkr.ecr.{}.amazonaws.com",
            config.account_id, config.region
        );
        let registry_url = format!(
            "{}/{}",
            registry_host,
            config.repo_prefix.trim_end_matches('/')
        );
        // We need *some* sts_client. Use a default empty config — no calls will be made.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let aws_config = rt.block_on(async {
            aws_config::defaults(BehaviorVersion::latest())
                .region(aws_config::Region::new(config.region.clone()))
                .load()
                .await
        });
        EcrProvider {
            config,
            sts_client: StsClient::new(&aws_config),
            registry_url,
            registry_host,
            cached_pull_creds: RwLock::new(None),
        }
    }

    #[test]
    fn external_image_allowed_without_strict_hosts() {
        let p = provider(vec![]);
        p.validate_image_for_project("nginx:latest", "anything")
            .unwrap();
        p.validate_image_for_project("jfrog.helsing-dev.ai/path/compass:v1", "evil")
            .unwrap();
    }

    #[test]
    fn external_image_not_in_strict_list_allowed() {
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        // Different host → still falls through as third-party
        p.validate_image_for_project("ghcr.io/owner/foo:v1", "anything")
            .unwrap();
    }

    #[test]
    fn strict_host_matching_project_allowed() {
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        p.validate_image_for_project(
            "jfrog.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps/compass:v1",
            "compass",
        )
        .unwrap();
    }

    #[test]
    fn strict_host_mismatched_project_rejected() {
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        let err = p
            .validate_image_for_project(
                "jfrog.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps/compass:v1",
                "evil",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("'evil'") && err.to_string().contains("'compass'"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn strict_host_with_digest_rejected_cross_project() {
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        let err = p
            .validate_image_for_project(
                "jfrog.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps/compass@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_uppercase_rejected_cross_project() {
        // Attacker-controlled `image` could otherwise dodge the strict
        // policy via host case-variation if the comparison were exact.
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        let err = p
            .validate_image_for_project(
                "JFROG.helsing-dev.ai/hdf-docker-playground/hs-hdf-rise-apps/compass:v1",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_with_default_port_rejected_cross_project() {
        // `:443` is the implicit HTTPS port — pulls would still succeed,
        // so we must normalize before comparing against the strict list.
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        let err = p
            .validate_image_for_project(
                "jfrog.helsing-dev.ai:443/hdf-docker-playground/hs-hdf-rise-apps/compass@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }

    #[test]
    fn strict_host_ported_untagged_ref_matched() {
        // Host:port format with no tag — must not treat `:443` as a tag.
        let p = provider(vec!["jfrog.helsing-dev.ai".to_string()]);
        p.validate_image_for_project(
            "jfrog.helsing-dev.ai:443/hdf-docker-playground/hs-hdf-rise-apps/compass",
            "compass",
        )
        .unwrap();
    }

    #[test]
    fn ecr_image_with_matching_project_allowed() {
        let p = provider(vec![]);
        p.validate_image_for_project(
            "123456789012.dkr.ecr.eu-west-1.amazonaws.com/rise/compass:v1",
            "compass",
        )
        .unwrap();
    }

    #[test]
    fn ecr_image_with_mismatched_project_rejected() {
        let p = provider(vec![]);
        let err = p
            .validate_image_for_project(
                "123456789012.dkr.ecr.eu-west-1.amazonaws.com/rise/compass:v1",
                "evil",
            )
            .unwrap_err();
        assert!(err.to_string().contains("'compass'"));
    }
}
