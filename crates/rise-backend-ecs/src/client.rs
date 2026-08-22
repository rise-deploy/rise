//! AWS client construction for the ECS backend.
//!
//! Mirrors the credential handling the rest of the tree already uses for ECR,
//! KMS and RDS: the standard credential chain by default (so a task role or
//! instance profile is the production path), with explicit static keys for
//! control planes running outside AWS.

use aws_sdk_ecs::config::Credentials;

/// Connection settings shared by every AWS client this backend builds.
pub struct AwsConfig<'a> {
    pub region: &'a str,
    /// Override the service endpoint. Serves VPC/PrivateLink and FIPS endpoints
    /// in production; the AWS Rust SDK also honours `AWS_ENDPOINT_URL`, so this
    /// is the explicit form of the same knob.
    pub endpoint_url: Option<&'a str>,
    pub access_key_id: Option<&'a str>,
    pub secret_access_key: Option<&'a str>,
}

/// Load an AWS SDK config from the supplied settings.
pub async fn load(cfg: &AwsConfig<'_>) -> aws_config::SdkConfig {
    let mut builder = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(cfg.region.to_string()));

    if let (Some(key_id), Some(secret_key)) = (cfg.access_key_id, cfg.secret_access_key) {
        builder = builder.credentials_provider(Credentials::new(
            key_id,
            secret_key,
            None,
            None,
            "static-credentials",
        ));
    }
    if let Some(endpoint) = cfg.endpoint_url.filter(|e| !e.trim().is_empty()) {
        builder = builder.endpoint_url(endpoint);
    }
    builder.load().await
}

/// Build an ECS client.
pub fn ecs(config: &aws_config::SdkConfig) -> aws_sdk_ecs::Client {
    aws_sdk_ecs::Client::new(config)
}

/// Build an SSM client (secret env-var parameters).
pub fn ssm(config: &aws_config::SdkConfig) -> aws_sdk_ssm::Client {
    aws_sdk_ssm::Client::new(config)
}
