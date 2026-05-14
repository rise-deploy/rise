use crate::db::{extensions as db_extensions, projects as db_projects};
use crate::server::encryption::EncryptionProvider;
use crate::server::extensions::{Extension, InjectedEnvVar, InjectedEnvVarValue};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_iam::Client as IamClient;
use aws_sdk_s3::Client as S3Client;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::db::leader_leases::LeaderElection;

const ENV_VAR_BUCKET_NAME: &str = "S3_BUCKET_NAME";
const ENV_VAR_ACCESS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
const ENV_VAR_SECRET_ACCESS_KEY: &str = "AWS_SECRET_ACCESS_KEY";
const ENV_VAR_REGION: &str = "AWS_REGION";

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AwsS3Spec {
    #[serde(default)]
    pub force_empty_bucket: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum S3BucketState {
    Pending,
    Creating,
    Available,
    Deleting,
    #[serde(rename = "deletion_blocked")]
    DeletionBlocked,
    Deleted,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AwsS3Status {
    pub state: S3BucketState,
    /// Random suffix included in AWS resource names to prevent collisions between
    /// projects whose names sanitize to the same value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iam_user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iam_access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iam_secret_access_key_encrypted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct AwsS3ProvisionerConfig {
    pub s3_client: S3Client,
    pub iam_client: IamClient,
    pub db_pool: sqlx::PgPool,
    pub encryption_provider: Arc<dyn EncryptionProvider>,
    pub region: String,
    pub bucket_prefix: String,
    pub bucket_name_template: String,
    /// ARN of the IAM permissions boundary policy (created by Terraform).
    /// All IAM users created by this extension will have this boundary attached,
    /// preventing them from being granted permissions beyond what the boundary allows.
    pub user_permissions_boundary_arn: String,
}

pub struct AwsS3Provisioner {
    s3_client: S3Client,
    iam_client: IamClient,
    db_pool: sqlx::PgPool,
    encryption_provider: Arc<dyn EncryptionProvider>,
    region: String,
    bucket_prefix: String,
    bucket_name_template: String,
    user_permissions_boundary_arn: String,
}

impl AwsS3Provisioner {
    pub fn new(config: AwsS3ProvisionerConfig) -> Self {
        Self {
            s3_client: config.s3_client,
            iam_client: config.iam_client,
            db_pool: config.db_pool,
            encryption_provider: config.encryption_provider,
            region: config.region,
            bucket_prefix: config.bucket_prefix,
            bucket_name_template: config.bucket_name_template,
            user_permissions_boundary_arn: config.user_permissions_boundary_arn,
        }
    }

    fn bucket_name_for(
        &self,
        project_name: &str,
        extension_name: &str,
        resource_id: &str,
    ) -> String {
        let raw = self
            .bucket_name_template
            .replace("{prefix}", &self.bucket_prefix)
            .replace("{project_name}", project_name)
            .replace("{extension_name}", extension_name);
        let sanitized_middle = sanitize_s3_name(&raw, raw.len());

        // resource_id suffix is always "-{id}" (9 chars for 8-hex-digit ID)
        let suffix = format!("-{}", resource_id);
        let max_middle = 63 - suffix.len();
        let truncated = if sanitized_middle.len() > max_middle {
            sanitized_middle[..max_middle]
                .trim_end_matches('-')
                .to_string()
        } else {
            sanitized_middle
        };

        format!("{}{}", truncated, suffix)
    }

    fn iam_user_name_for(
        &self,
        project_name: &str,
        extension_name: &str,
        resource_id: &str,
    ) -> String {
        let middle = format!(
            "{}-s3-{}-{}",
            self.bucket_prefix, project_name, extension_name
        );
        let sanitized_middle = sanitize_iam_name(&middle, middle.len());

        let suffix = format!("-{}", resource_id);
        let max_middle = 64 - suffix.len();
        let truncated = if sanitized_middle.len() > max_middle {
            sanitized_middle[..max_middle]
                .trim_end_matches('-')
                .to_string()
        } else {
            sanitized_middle
        };

        format!("{}{}", truncated, suffix)
    }

    fn generate_resource_id() -> String {
        use rand::RngExt;
        let mut rng = rand::rng();
        format!("{:08x}", rng.random::<u32>())
    }

    fn finalizer_name(&self, extension_name: &str) -> String {
        format!(
            "rise.dev/extension/{}/{}",
            self.extension_type(),
            extension_name
        )
    }

    async fn handle_pending(
        &self,
        status: &mut AwsS3Status,
        project_name: &str,
        project_id: Uuid,
        extension_name: &str,
    ) -> Result<()> {
        let resource_id = status
            .resource_id
            .get_or_insert_with(Self::generate_resource_id)
            .clone();
        let bucket_name = self.bucket_name_for(project_name, extension_name, &resource_id);
        let iam_user_name = self.iam_user_name_for(project_name, extension_name, &resource_id);

        info!(
            "Provisioning S3 bucket '{}' and IAM user '{}' for project '{}' (extension: '{}')",
            bucket_name, iam_user_name, project_name, extension_name
        );

        // 1. Create S3 bucket
        let create_result = if self.region == "us-east-1" {
            // us-east-1 does not accept a LocationConstraint
            self.s3_client
                .create_bucket()
                .bucket(&bucket_name)
                .send()
                .await
        } else {
            let constraint = aws_sdk_s3::types::CreateBucketConfiguration::builder()
                .location_constraint(aws_sdk_s3::types::BucketLocationConstraint::from(
                    self.region.as_str(),
                ))
                .build();
            self.s3_client
                .create_bucket()
                .bucket(&bucket_name)
                .create_bucket_configuration(constraint)
                .send()
                .await
        };

        match create_result {
            Ok(_) => {
                info!("Created S3 bucket '{}'", bucket_name);
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("BucketAlreadyOwnedByYou") {
                    info!(
                        "S3 bucket '{}' already exists and is owned by this account, continuing",
                        bucket_name
                    );
                } else {
                    error!("Failed to create S3 bucket '{}': {:?}", bucket_name, e);
                    status.state = S3BucketState::Failed;
                    status.error = Some(format!("Failed to create bucket: {:?}", e));
                    return Ok(());
                }
            }
        }

        // 2. Block all public access
        if let Err(e) = self
            .s3_client
            .put_public_access_block()
            .bucket(&bucket_name)
            .public_access_block_configuration(
                aws_sdk_s3::types::PublicAccessBlockConfiguration::builder()
                    .block_public_acls(true)
                    .ignore_public_acls(true)
                    .block_public_policy(true)
                    .restrict_public_buckets(true)
                    .build(),
            )
            .send()
            .await
        {
            warn!(
                "Failed to configure public access block for bucket '{}': {:?}",
                bucket_name, e
            );
        }

        // 3. Tag bucket
        let tags = aws_sdk_s3::types::Tagging::builder()
            .tag_set(
                aws_sdk_s3::types::Tag::builder()
                    .key("rise:managed")
                    .value("true")
                    .build()
                    .expect("valid tag"),
            )
            .tag_set(
                aws_sdk_s3::types::Tag::builder()
                    .key("rise:project")
                    .value(project_name)
                    .build()
                    .expect("valid tag"),
            )
            .build()
            .expect("valid tagging");

        if let Err(e) = self
            .s3_client
            .put_bucket_tagging()
            .bucket(&bucket_name)
            .tagging(tags)
            .send()
            .await
        {
            warn!("Failed to tag S3 bucket '{}': {:?}", bucket_name, e);
        }

        // 4. Create IAM user with permissions boundary
        match self
            .iam_client
            .create_user()
            .user_name(&iam_user_name)
            .permissions_boundary(&self.user_permissions_boundary_arn)
            .tags(
                aws_sdk_iam::types::Tag::builder()
                    .key("rise:managed")
                    .value("true")
                    .build()
                    .expect("valid tag"),
            )
            .tags(
                aws_sdk_iam::types::Tag::builder()
                    .key("rise:project")
                    .value(project_name)
                    .build()
                    .expect("valid tag"),
            )
            .send()
            .await
        {
            Ok(_) => {
                info!("Created IAM user '{}'", iam_user_name);
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("EntityAlreadyExists") {
                    info!("IAM user '{}' already exists, continuing", iam_user_name);
                } else {
                    error!("Failed to create IAM user '{}': {:?}", iam_user_name, e);
                    status.state = S3BucketState::Failed;
                    status.error = Some(format!("Failed to create IAM user: {:?}", e));
                    return Ok(());
                }
            }
        }

        // 5. Attach inline policy granting full access to the specific bucket
        let policy_doc = serde_json::json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Sid": "S3BucketFullAccess",
                "Effect": "Allow",
                "Action": ["s3:*"],
                "Resource": [
                    format!("arn:aws:s3:::{}", bucket_name),
                    format!("arn:aws:s3:::{}/*", bucket_name)
                ]
            }]
        });

        if let Err(e) = self
            .iam_client
            .put_user_policy()
            .user_name(&iam_user_name)
            .policy_name("rise-s3-access")
            .policy_document(policy_doc.to_string())
            .send()
            .await
        {
            error!(
                "Failed to attach inline policy to IAM user '{}': {:?}",
                iam_user_name, e
            );
            status.state = S3BucketState::Failed;
            status.error = Some(format!("Failed to attach IAM policy: {:?}", e));
            return Ok(());
        }

        info!("Attached inline S3 policy to IAM user '{}'", iam_user_name);

        // 6. Create access key
        match self
            .iam_client
            .create_access_key()
            .user_name(&iam_user_name)
            .send()
            .await
        {
            Ok(resp) => {
                let key = resp
                    .access_key()
                    .ok_or_else(|| anyhow::anyhow!("No access key in response"))?;

                let access_key_id = key.access_key_id().to_string();
                let secret_access_key = key.secret_access_key().to_string();

                // 7. Encrypt the secret key
                let encrypted_secret = self
                    .encryption_provider
                    .encrypt(&secret_access_key)
                    .await
                    .context("Failed to encrypt IAM secret access key")?;

                // 8. Store everything in status and add finalizer
                status.state = S3BucketState::Available;
                status.bucket_name = Some(bucket_name.clone());
                status.iam_user_name = Some(iam_user_name.clone());
                status.iam_access_key_id = Some(access_key_id);
                status.iam_secret_access_key_encrypted = Some(encrypted_secret);
                status.region = Some(self.region.clone());
                status.error = None;

                let finalizer = self.finalizer_name(extension_name);
                if let Err(e) =
                    db_projects::add_finalizer(&self.db_pool, project_id, &finalizer).await
                {
                    error!(
                        "Failed to add finalizer '{}' to project '{}': {:?}",
                        finalizer, project_name, e
                    );
                } else {
                    info!(
                        "Added finalizer '{}' to project '{}'",
                        finalizer, project_name
                    );
                }

                info!(
                    "S3 bucket '{}' and IAM user '{}' are now available",
                    bucket_name, iam_user_name
                );
            }
            Err(e) => {
                error!(
                    "Failed to create access key for IAM user '{}': {:?}",
                    iam_user_name, e
                );
                status.state = S3BucketState::Failed;
                status.error = Some(format!("Failed to create access key: {:?}", e));
            }
        }

        Ok(())
    }

    async fn handle_available(&self, status: &mut AwsS3Status) -> Result<()> {
        let bucket_name = match &status.bucket_name {
            Some(n) => n.clone(),
            None => {
                warn!("S3 extension in Available state but bucket_name is missing");
                return Ok(());
            }
        };
        let iam_user_name = match &status.iam_user_name {
            Some(n) => n.clone(),
            None => {
                warn!("S3 extension in Available state but iam_user_name is missing");
                return Ok(());
            }
        };

        // Verify bucket still exists
        match self
            .s3_client
            .head_bucket()
            .bucket(&bucket_name)
            .send()
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("NoSuchBucket") || err_str.contains("NotFound") {
                    warn!(
                        "S3 bucket '{}' no longer exists, marking for re-creation",
                        bucket_name
                    );
                    status.state = S3BucketState::Pending;
                    status.bucket_name = None;
                    status.iam_user_name = None;
                    status.iam_access_key_id = None;
                    status.iam_secret_access_key_encrypted = None;
                } else {
                    warn!("Failed to verify S3 bucket '{}': {:?}", bucket_name, e);
                }
                return Ok(());
            }
        }

        // Verify IAM user still exists
        match self
            .iam_client
            .get_user()
            .user_name(&iam_user_name)
            .send()
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("NoSuchEntity") {
                    warn!(
                        "IAM user '{}' no longer exists, marking for re-creation",
                        iam_user_name
                    );
                    status.state = S3BucketState::Pending;
                    status.iam_user_name = None;
                    status.iam_access_key_id = None;
                    status.iam_secret_access_key_encrypted = None;
                } else {
                    warn!("Failed to verify IAM user '{}': {:?}", iam_user_name, e);
                }
                return Ok(());
            }
        }

        // Verify access key exists; if not, rotate credentials
        let access_key_id = match &status.iam_access_key_id {
            Some(k) => k.clone(),
            None => {
                info!(
                    "Access key missing for IAM user '{}', generating new key",
                    iam_user_name
                );
                self.rotate_access_key(status, &iam_user_name).await?;
                return Ok(());
            }
        };

        // Check if our stored key still exists on AWS
        match self
            .iam_client
            .list_access_keys()
            .user_name(&iam_user_name)
            .send()
            .await
        {
            Ok(resp) => {
                let key_exists = resp
                    .access_key_metadata()
                    .iter()
                    .any(|k| k.access_key_id() == Some(access_key_id.as_str()));

                if !key_exists {
                    info!(
                        "Access key '{}' no longer exists for IAM user '{}', generating new key",
                        access_key_id, iam_user_name
                    );
                    self.rotate_access_key(status, &iam_user_name).await?;
                }
            }
            Err(e) => {
                warn!(
                    "Failed to list access keys for IAM user '{}': {:?}",
                    iam_user_name, e
                );
            }
        }

        Ok(())
    }

    async fn rotate_access_key(&self, status: &mut AwsS3Status, iam_user_name: &str) -> Result<()> {
        // Delete all existing keys first
        if let Ok(resp) = self
            .iam_client
            .list_access_keys()
            .user_name(iam_user_name)
            .send()
            .await
        {
            for key_meta in resp.access_key_metadata() {
                if let Some(key_id) = key_meta.access_key_id() {
                    let _ = self
                        .iam_client
                        .delete_access_key()
                        .user_name(iam_user_name)
                        .access_key_id(key_id)
                        .send()
                        .await;
                }
            }
        }

        match self
            .iam_client
            .create_access_key()
            .user_name(iam_user_name)
            .send()
            .await
        {
            Ok(resp) => {
                let key = resp
                    .access_key()
                    .ok_or_else(|| anyhow::anyhow!("No access key in response"))?;

                let encrypted_secret = self
                    .encryption_provider
                    .encrypt(key.secret_access_key())
                    .await
                    .context("Failed to encrypt new secret access key")?;

                status.iam_access_key_id = Some(key.access_key_id().to_string());
                status.iam_secret_access_key_encrypted = Some(encrypted_secret);

                info!("Rotated access key for IAM user '{}'", iam_user_name);
            }
            Err(e) => {
                error!(
                    "Failed to create new access key for IAM user '{}': {:?}",
                    iam_user_name, e
                );
            }
        }

        Ok(())
    }

    /// Delete one batch of object versions and delete markers from the bucket.
    /// Returns `true` if the bucket is now empty (no versions/markers remain).
    async fn empty_bucket_batch(&self, bucket_name: &str) -> Result<bool> {
        let resp = self
            .s3_client
            .list_object_versions()
            .bucket(bucket_name)
            .max_keys(1000)
            .send()
            .await
            .context("Failed to list object versions")?;

        let mut objects_to_delete: Vec<aws_sdk_s3::types::ObjectIdentifier> = Vec::new();

        for version in resp.versions() {
            if let (Some(key), Some(vid)) = (version.key(), version.version_id()) {
                objects_to_delete.push(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key(key)
                        .version_id(vid)
                        .build()
                        .context("Failed to build ObjectIdentifier")?,
                );
            }
        }

        for marker in resp.delete_markers() {
            if let (Some(key), Some(vid)) = (marker.key(), marker.version_id()) {
                objects_to_delete.push(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key(key)
                        .version_id(vid)
                        .build()
                        .context("Failed to build ObjectIdentifier")?,
                );
            }
        }

        if objects_to_delete.is_empty() {
            return Ok(true);
        }

        let count = objects_to_delete.len();
        let delete = aws_sdk_s3::types::Delete::builder()
            .set_objects(Some(objects_to_delete))
            .build()
            .context("Failed to build Delete request")?;

        self.s3_client
            .delete_objects()
            .bucket(bucket_name)
            .delete(delete)
            .send()
            .await
            .context("Failed to delete object versions")?;

        info!(
            "Deleted {} object version(s) from S3 bucket '{}'",
            count, bucket_name
        );

        // If we got a full page, there are likely more objects remaining
        Ok(count < 1000)
    }

    async fn handle_deletion(
        &self,
        status: &mut AwsS3Status,
        project_name: &str,
        spec: &AwsS3Spec,
        election: &LeaderElection,
    ) -> Result<()> {
        status.state = S3BucketState::Deleting;
        status.error = None;

        // 1. Delete IAM access keys, policy, and user
        if let Some(ref iam_user_name) = status.iam_user_name.clone() {
            match self
                .iam_client
                .list_access_keys()
                .user_name(iam_user_name)
                .send()
                .await
            {
                Ok(resp) => {
                    for key_meta in resp.access_key_metadata() {
                        let Some(key_id) = key_meta.access_key_id() else {
                            continue;
                        };
                        if let Err(e) = self
                            .iam_client
                            .delete_access_key()
                            .user_name(iam_user_name)
                            .access_key_id(key_id)
                            .send()
                            .await
                        {
                            warn!(
                                "Failed to delete access key '{}' for IAM user '{}': {:?}",
                                key_id, iam_user_name, e
                            );
                        }
                    }
                }
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if !err_str.contains("NoSuchEntity") {
                        warn!(
                            "Failed to list access keys for IAM user '{}': {:?}",
                            iam_user_name, e
                        );
                    }
                }
            }

            let _ = self
                .iam_client
                .delete_user_policy()
                .user_name(iam_user_name)
                .policy_name("rise-s3-access")
                .send()
                .await;

            match self
                .iam_client
                .delete_user()
                .user_name(iam_user_name)
                .send()
                .await
            {
                Ok(_) => {
                    info!("Deleted IAM user '{}'", iam_user_name);
                }
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if err_str.contains("NoSuchEntity") {
                        info!("IAM user '{}' already deleted", iam_user_name);
                    } else {
                        warn!("Failed to delete IAM user '{}': {:?}", iam_user_name, e);
                    }
                }
            }

            status.iam_user_name = None;
            status.iam_access_key_id = None;
            status.iam_secret_access_key_encrypted = None;
        }

        // 2. Delete S3 bucket (only if empty)
        if let Some(ref bucket_name) = status.bucket_name.clone() {
            let is_empty = match self
                .s3_client
                .list_objects_v2()
                .bucket(bucket_name)
                .max_keys(1)
                .send()
                .await
            {
                Ok(resp) => resp.key_count().unwrap_or(0) == 0,
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if err_str.contains("NoSuchBucket") {
                        info!("S3 bucket '{}' already deleted", bucket_name);
                        status.state = S3BucketState::Deleted;
                        return Ok(());
                    }
                    let msg = format!(
                        "Failed to check if S3 bucket '{}' is empty: {:?}",
                        bucket_name, e
                    );
                    warn!("{}", msg);
                    status.error = Some(msg);
                    return Ok(());
                }
            };

            if is_empty {
                match self
                    .s3_client
                    .delete_bucket()
                    .bucket(bucket_name)
                    .send()
                    .await
                {
                    Ok(_) => {
                        info!("Deleted S3 bucket '{}'", bucket_name);
                    }
                    Err(e) => {
                        let err_str = format!("{:?}", e);
                        if err_str.contains("NoSuchBucket") {
                            info!("S3 bucket '{}' already deleted", bucket_name);
                        } else {
                            let msg =
                                format!("Failed to delete S3 bucket '{}': {:?}", bucket_name, e);
                            warn!("{}", msg);
                            status.error = Some(msg);
                            return Ok(());
                        }
                    }
                }
                status.bucket_name = None;
            } else if spec.force_empty_bucket {
                info!(
                    "Force-emptying S3 bucket '{}' for project '{}'",
                    bucket_name, project_name
                );
                match self.empty_bucket_batch(bucket_name).await {
                    Ok(now_empty) => {
                        election.assert_leader().await?;
                        if now_empty {
                            // Bucket is empty — delete it now
                            match self
                                .s3_client
                                .delete_bucket()
                                .bucket(bucket_name)
                                .send()
                                .await
                            {
                                Ok(_) => {
                                    info!("Deleted S3 bucket '{}'", bucket_name);
                                    status.bucket_name = None;
                                }
                                Err(e) => {
                                    let err_str = format!("{:?}", e);
                                    if err_str.contains("NoSuchBucket") {
                                        info!("S3 bucket '{}' already deleted", bucket_name);
                                        status.bucket_name = None;
                                    } else {
                                        let msg = format!(
                                            "Failed to delete S3 bucket '{}': {:?}",
                                            bucket_name, e
                                        );
                                        warn!("{}", msg);
                                        status.error = Some(msg);
                                        return Ok(());
                                    }
                                }
                            }
                        } else {
                            // More objects remain — stay in Deleting, continue next tick
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        let msg = format!("Failed to empty S3 bucket '{}': {:?}", bucket_name, e);
                        warn!("{}", msg);
                        status.error = Some(msg);
                        return Ok(());
                    }
                }
            } else {
                let msg = format!(
                    "S3 bucket '{}' for project '{}' is not empty — \
                     the bucket must be emptied before the extension can be fully removed.",
                    bucket_name, project_name
                );
                warn!("{}", msg);
                status.state = S3BucketState::DeletionBlocked;
                status.error = Some(msg);
                return Ok(());
            }
        }

        status.state = S3BucketState::Deleted;

        Ok(())
    }

    async fn reconcile_single(
        &self,
        project_extension: crate::db::models::ProjectExtension,
        election: &LeaderElection,
    ) -> Result<bool> {
        debug!(
            "Reconciling AWS S3 extension: project={}, extension={}",
            project_extension.project_id, project_extension.extension
        );

        let project = db_projects::find_by_id(&self.db_pool, project_extension.project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Project not found"))?;

        let mut status: AwsS3Status = serde_json::from_value(project_extension.status.clone())
            .unwrap_or(AwsS3Status {
                state: S3BucketState::Pending,
                resource_id: None,
                bucket_name: None,
                iam_user_name: None,
                iam_access_key_id: None,
                iam_secret_access_key_encrypted: None,
                region: None,
                error: None,
            });

        let spec: AwsS3Spec =
            serde_json::from_value(project_extension.spec.clone()).unwrap_or_default();

        if project_extension.deleted_at.is_some() {
            if status.state != S3BucketState::Deleted {
                if status.state == S3BucketState::DeletionBlocked && spec.force_empty_bucket {
                    info!(
                        "Force-empty enabled for S3 extension '{}' on project '{}', resuming deletion",
                        project_extension.extension, project.name
                    );
                    status.state = S3BucketState::Deleting;
                } else if !matches!(
                    status.state,
                    S3BucketState::Deleting | S3BucketState::DeletionBlocked
                ) {
                    info!(
                        "Beginning cleanup for S3 extension '{}' on project '{}'",
                        project_extension.extension, project.name
                    );
                }

                if status.state != S3BucketState::DeletionBlocked {
                    self.handle_deletion(&mut status, &project.name, &spec, election)
                        .await?;
                }

                election.assert_leader().await?;
                db_extensions::update_status(
                    &self.db_pool,
                    project_extension.project_id,
                    &project_extension.extension,
                    &serde_json::to_value(&status)?,
                )
                .await?;

                if status.state == S3BucketState::Deleted {
                    let finalizer = self.finalizer_name(&project_extension.extension);

                    election.assert_leader().await?;
                    if let Err(e) = db_projects::remove_finalizer(
                        &self.db_pool,
                        project_extension.project_id,
                        &finalizer,
                    )
                    .await
                    {
                        error!(
                            "Failed to remove finalizer '{}' from project '{}': {:?}",
                            finalizer, project.name, e
                        );
                    } else {
                        info!(
                            "Removed finalizer '{}' from project '{}'",
                            finalizer, project.name
                        );
                    }

                    election.assert_leader().await?;
                    db_extensions::delete_permanently(
                        &self.db_pool,
                        project_extension.project_id,
                        &project_extension.extension,
                    )
                    .await?;
                    info!(
                        "Permanently deleted S3 extension record for project '{}'",
                        project.name
                    );
                }
            }

            return Ok(false);
        }

        let initial_state = status.state.clone();

        match status.state {
            S3BucketState::Pending => {
                self.handle_pending(
                    &mut status,
                    &project.name,
                    project.id,
                    &project_extension.extension,
                )
                .await?;
            }
            S3BucketState::Creating => {
                // Creating is not a separate async phase for S3 (unlike RDS which polls until
                // the instance is ready). The handle_pending function transitions directly to
                // Available. This state can only be reached if the process was interrupted
                // mid-provisioning — treat it as Pending and retry.
                info!(
                    "S3 extension for project '{}' stuck in Creating state, retrying from Pending",
                    project.name
                );
                status.state = S3BucketState::Pending;
            }
            S3BucketState::Available => {
                self.handle_available(&mut status).await?;
            }
            S3BucketState::Failed => {
                info!(
                    "S3 extension for project '{}' is in Failed state, retrying",
                    project.name
                );
                status.state = S3BucketState::Pending;
                status.error = None;
            }
            _ => {}
        }

        election.assert_leader().await?;
        db_extensions::update_status(
            &self.db_pool,
            project_extension.project_id,
            &project_extension.extension,
            &serde_json::to_value(&status)?,
        )
        .await?;

        Ok(status.state != initial_state)
    }
}

/// Sanitize a string to be a valid S3 bucket name:
/// lowercase, only alphanumeric and hyphens, start/end with alphanumeric,
/// truncated to `max_len`.
fn sanitize_s3_name(raw: &str, max_len: usize) -> String {
    let lower = raw.to_lowercase();
    let sanitized: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive hyphens and strip leading/trailing hyphens
    let mut result = String::new();
    let mut prev_hyphen = true; // treat start as hyphen to strip leading ones
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
                prev_hyphen = true;
            }
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    // Strip trailing hyphen
    let result = result.trim_end_matches('-').to_string();
    result.chars().take(max_len).collect()
}

/// Sanitize a string to be a valid IAM user name (alphanumeric, hyphens, underscores, plus, equals,
/// comma, period, @), truncated to `max_len`.
fn sanitize_iam_name(raw: &str, max_len: usize) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    sanitized.chars().take(max_len).collect()
}

#[async_trait]
impl Extension for AwsS3Provisioner {
    fn extension_type(&self) -> &str {
        "aws-s3-bucket"
    }

    fn display_name(&self) -> &str {
        "AWS S3 Bucket"
    }

    async fn validate_spec(&self, spec: &Value) -> Result<()> {
        serde_json::from_value::<AwsS3Spec>(spec.clone()).context("Failed to parse AWS S3 spec")?;
        Ok(())
    }

    fn start(&self) {
        let provisioner = Self {
            s3_client: self.s3_client.clone(),
            iam_client: self.iam_client.clone(),
            db_pool: self.db_pool.clone(),
            encryption_provider: self.encryption_provider.clone(),
            region: self.region.clone(),
            bucket_prefix: self.bucket_prefix.clone(),
            bucket_name_template: self.bucket_name_template.clone(),
            user_permissions_boundary_arn: self.user_permissions_boundary_arn.clone(),
        };

        tokio::spawn(async move {
            info!(
                "Starting AWS S3 extension reconciliation loop for type '{}'",
                provisioner.extension_type()
            );

            let election = LeaderElection::spawn(
                provisioner.db_pool.clone(),
                "rise-ext-s3",
                Uuid::new_v4(),
                std::time::Duration::from_secs(60),
            );

            let mut error_state: HashMap<Uuid, (usize, DateTime<Utc>)> = HashMap::new();

            loop {
                if !election.is_leader() {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                match db_extensions::list_by_extension_type(
                    &provisioner.db_pool,
                    provisioner.extension_type(),
                )
                .await
                {
                    Ok(extensions) => {
                        if extensions.is_empty() {
                            debug!("No S3 extensions found, waiting for work");
                        }

                        for ext in extensions {
                            let ext_status: Option<AwsS3Status> =
                                serde_json::from_value(ext.status.clone()).ok();
                            let ext_spec: AwsS3Spec =
                                serde_json::from_value(ext.spec.clone()).unwrap_or_default();
                            debug!(
                                "S3 extension '{}' project={}: state={:?}, deleted={}, force_empty={}",
                                ext.extension,
                                ext.project_id,
                                ext_status.as_ref().map(|s| &s.state),
                                ext.deleted_at.is_some(),
                                ext_spec.force_empty_bucket,
                            );

                            if let Some((error_count, last_error)) =
                                error_state.get(&ext.project_id)
                            {
                                let backoff_seconds = 2_i64.pow(*error_count as u32).min(300);
                                let backoff_until =
                                    *last_error + Duration::seconds(backoff_seconds);

                                if Utc::now() < backoff_until {
                                    debug!(
                                        "Skipping S3 extension '{}' for project {} due to backoff ({} errors, {}s remaining)",
                                        ext.extension,
                                        ext.project_id,
                                        error_count,
                                        (backoff_until - Utc::now()).num_seconds(),
                                    );
                                    continue;
                                }
                            }

                            if !election.is_leader() {
                                warn!("Lost leadership before S3 reconcile");
                                break;
                            }

                            match provisioner.reconcile_single(ext.clone(), &election).await {
                                Ok(_) => {
                                    error_state.remove(&ext.project_id);
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to reconcile AWS S3 extension for project {}: {:?}",
                                        ext.project_id, e
                                    );
                                    let entry = error_state
                                        .entry(ext.project_id)
                                        .or_insert((0, Utc::now()));
                                    entry.0 += 1;
                                    entry.1 = Utc::now();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to list S3 extensions: {:?}", e);
                    }
                }

                let needs_active_polling = match db_extensions::list_by_extension_type(
                    &provisioner.db_pool,
                    provisioner.extension_type(),
                )
                .await
                {
                    Ok(extensions) => extensions.iter().any(|ext| {
                        if let Ok(status) =
                            serde_json::from_value::<AwsS3Status>(ext.status.clone())
                        {
                            let spec: AwsS3Spec =
                                serde_json::from_value(ext.spec.clone()).unwrap_or_default();
                            matches!(
                                status.state,
                                S3BucketState::Pending
                                    | S3BucketState::Creating
                                    | S3BucketState::Failed
                            ) || (ext.deleted_at.is_some()
                                && (status.state != S3BucketState::DeletionBlocked
                                    || spec.force_empty_bucket))
                        } else {
                            false
                        }
                    }),
                    Err(_) => false,
                };

                let wait_time = if needs_active_polling { 2 } else { 10 };
                sleep(std::time::Duration::from_secs(wait_time)).await;
            }
        });
    }

    async fn before_deployment(
        &self,
        project_id: Uuid,
        _deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        let extensions =
            db_extensions::list_by_extension_type(&self.db_pool, self.extension_type())
                .await?
                .into_iter()
                .filter(|e| e.project_id == project_id && e.deleted_at.is_none())
                .collect::<Vec<_>>();

        if extensions.is_empty() {
            debug!(
                "S3 extension not enabled for project {}, skipping before_deployment hook",
                project_id
            );
            return Ok(vec![]);
        }

        let ext = &extensions[0];
        if extensions.len() > 1 {
            warn!(
                "Multiple S3 extensions found for project {}, using first: {}",
                project_id, ext.extension
            );
        }

        let status: AwsS3Status =
            serde_json::from_value(ext.status.clone()).context("Failed to parse S3 status")?;

        if status.state != S3BucketState::Available {
            anyhow::bail!(
                "S3 extension '{}' is not yet available (current state: {:?}). \
                 Wait for provisioning to complete before deploying.",
                ext.extension,
                status.state
            );
        }

        let bucket_name = status
            .bucket_name
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("S3 bucket name not set"))?;

        let access_key_id = status
            .iam_access_key_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("IAM access key ID not set"))?;

        let secret_encrypted = status
            .iam_secret_access_key_encrypted
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("IAM secret access key not set"))?;

        let secret_decrypted = self
            .encryption_provider
            .decrypt(secret_encrypted)
            .await
            .context("Failed to decrypt IAM secret access key")?;

        Ok(vec![
            InjectedEnvVar {
                key: ENV_VAR_BUCKET_NAME.to_string(),
                value: InjectedEnvVarValue::Plain(bucket_name.clone()),
            },
            InjectedEnvVar {
                key: ENV_VAR_ACCESS_KEY_ID.to_string(),
                value: InjectedEnvVarValue::Plain(access_key_id.clone()),
            },
            InjectedEnvVar {
                key: ENV_VAR_SECRET_ACCESS_KEY.to_string(),
                value: InjectedEnvVarValue::Protected {
                    decrypted: secret_decrypted,
                    encrypted: secret_encrypted.clone(),
                },
            },
            InjectedEnvVar {
                key: ENV_VAR_REGION.to_string(),
                value: InjectedEnvVarValue::Plain(self.region.clone()),
            },
        ])
    }

    async fn preview_env_vars(
        &self,
        _project_id: Uuid,
        _deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        // S3 credentials are sensitive; don't expose them in previews
        Ok(vec![])
    }

    fn format_status(&self, status: &Value) -> String {
        let Ok(parsed) = serde_json::from_value::<AwsS3Status>(status.clone()) else {
            return "Unknown".to_string();
        };

        match parsed.state {
            S3BucketState::Pending => "Pending".to_string(),
            S3BucketState::Creating => "Creating...".to_string(),
            S3BucketState::Available => {
                if let Some(bucket) = parsed.bucket_name {
                    format!("Available ({})", bucket)
                } else {
                    "Available".to_string()
                }
            }
            S3BucketState::Deleting => "Deleting...".to_string(),
            S3BucketState::DeletionBlocked => {
                if let Some(err) = parsed.error {
                    format!("Deletion blocked: {}", err)
                } else {
                    "Deletion blocked".to_string()
                }
            }
            S3BucketState::Deleted => "Deleted".to_string(),
            S3BucketState::Failed => {
                if let Some(err) = parsed.error {
                    format!("Failed: {}", err)
                } else {
                    "Failed".to_string()
                }
            }
        }
    }

    fn description(&self) -> &str {
        "Provisions an S3 bucket and injects full-access credentials as environment variables"
    }

    fn documentation(&self) -> &str {
        r#"# AWS S3 Bucket Extension

This extension provisions a dedicated S3 bucket for your project and automatically injects
full-access credentials as environment variables into every deployment.

## How it works

1. An S3 bucket is created with a name derived from your project and extension name.
2. A dedicated IAM user is created with an inline policy granting full access to **only** that bucket.
   The IAM user is created with a permissions boundary that prevents it from ever being granted
   permissions beyond S3 access on the Rise bucket prefix.
3. An IAM access key is generated for the user.
4. The credentials are securely injected into deployments as environment variables.

## Environment Variables

The following variables are injected into every deployment automatically:

| Variable | Description |
|---|---|
| `S3_BUCKET_NAME` | The name of the provisioned S3 bucket |
| `AWS_ACCESS_KEY_ID` | IAM access key ID |
| `AWS_SECRET_ACCESS_KEY` | IAM secret access key |
| `AWS_REGION` | AWS region where the bucket is located |

These variables are recognized by all AWS SDKs, the AWS CLI, and most S3-compatible libraries.

## Bucket Deletion

When you delete this extension, the associated IAM user and access key are removed immediately.
The S3 bucket is only deleted **if it is empty**. If the bucket contains objects, it will be
left in place to prevent data loss. You must empty and delete it manually in that case.

## Example Spec

```json
{}
```

No configuration is required for v0.
"#
    }

    fn spec_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "force_empty_bucket": {
                    "type": "boolean",
                    "default": false,
                    "description": "When enabled, the controller will empty the S3 bucket (including all object versions) before deleting it during extension removal."
                }
            },
            "description": "The extension injects S3_BUCKET_NAME, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, and AWS_REGION into deployments."
        })
    }
}
