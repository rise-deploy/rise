use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::Client as KmsClient;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use rise_backend_core::encryption::{decode_envelope, encode_envelope, CiphertextRejected};

/// AWS KMS encryption provider
/// Uses AWS KMS for encryption/decryption operations
pub struct AwsKmsEncryptionProvider {
    client: KmsClient,
    key_id: String,
}

impl AwsKmsEncryptionProvider {
    /// Create a new AWS KMS encryption provider
    pub async fn new(
        region: &str,
        key_id: String,
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
    ) -> Result<Self> {
        let config = if let (Some(access_key), Some(secret_key)) =
            (access_key_id, secret_access_key)
        {
            // Use static credentials (development only)
            let creds =
                aws_sdk_kms::config::Credentials::new(access_key, secret_key, None, None, "static");
            aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(region.to_string()))
                .credentials_provider(creds)
                .load()
                .await
        } else {
            // Use default credential chain (IRSA, instance profile, env vars, etc.)
            aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(region.to_string()))
                .load()
                .await
        };

        let client = KmsClient::new(&config);

        Ok(Self { client, key_id })
    }
}

#[async_trait]
impl EncryptionProvider for AwsKmsEncryptionProvider {
    async fn encrypt(&self, plaintext: &str, context: &EncryptionContext) -> Result<String> {
        let response = self
            .encrypt_request(plaintext, context)
            .send()
            .await
            .with_context(|| {
                format!(
                    "KMS encryption failed for key '{}'. Common causes: \
                     1) Invalid key ARN/ID, 2) No AWS credentials available, \
                     3) Insufficient IAM permissions (kms:Encrypt), \
                     4) Key is disabled or pending deletion",
                    self.key_id
                )
            })?;

        let ciphertext_blob = response
            .ciphertext_blob()
            .context("No ciphertext blob in KMS response")?;

        // Encode to base64 for storage in database
        Ok(encode_envelope(ciphertext_blob.as_ref()))
    }

    async fn decrypt(&self, envelope: &str, context: &EncryptionContext) -> Result<String> {
        self.decrypt_bytes(decode_envelope(envelope)?, Some(kms_context(context)))
            .await
    }

    async fn decrypt_legacy_for_migration(&self, ciphertext: &str) -> Result<String> {
        if ciphertext.starts_with("rise:") {
            anyhow::bail!("Unsupported baseline ciphertext version");
        }
        self.decrypt_bytes(
            BASE64
                .decode(ciphertext)
                .context("Invalid baseline encoding")?,
            None,
        )
        .await
    }
}

fn kms_context(context: &EncryptionContext) -> std::collections::HashMap<String, String> {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(context.authenticated_bytes());
    std::collections::HashMap::from([("rise:context:v2".into(), BASE64.encode(hash))])
}

impl AwsKmsEncryptionProvider {
    fn encrypt_request(
        &self,
        plaintext: &str,
        context: &EncryptionContext,
    ) -> aws_sdk_kms::operation::encrypt::builders::EncryptFluentBuilder {
        self.client
            .encrypt()
            .key_id(&self.key_id)
            .plaintext(Blob::new(plaintext.as_bytes()))
            .set_encryption_context(Some(kms_context(context)))
    }

    fn decrypt_request(
        &self,
        ciphertext: Vec<u8>,
        context: Option<std::collections::HashMap<String, String>>,
    ) -> aws_sdk_kms::operation::decrypt::builders::DecryptFluentBuilder {
        self.client
            .decrypt()
            .key_id(&self.key_id)
            .ciphertext_blob(Blob::new(ciphertext))
            .set_encryption_context(context)
    }

    async fn decrypt_bytes(
        &self,
        ciphertext: Vec<u8>,
        context: Option<std::collections::HashMap<String, String>>,
    ) -> Result<String> {
        let response = self
            .decrypt_request(ciphertext, context)
            .send()
            .await
            .map_err(decrypt_error)?;
        let plaintext = response
            .plaintext()
            .context("No plaintext in KMS response")?;
        String::from_utf8(plaintext.clone().into_inner())
            .context("Decrypted data is not valid UTF-8")
            .map_err(CiphertextRejected::tag)
    }
}

/// Tags the KMS verdicts on the ciphertext itself as [`CiphertextRejected`]; every other
/// failure (transport, timeout, throttling, a disabled or unavailable key, missing
/// permissions) stays untagged.
fn decrypt_error(
    error: aws_sdk_kms::error::SdkError<
        aws_sdk_kms::operation::decrypt::DecryptError,
        aws_sdk_kms::config::http::HttpResponse,
    >,
) -> anyhow::Error {
    let rejected = error.as_service_error().is_some_and(|e| {
        e.is_invalid_ciphertext_exception()
            || e.is_incorrect_key_exception()
            || e.is_invalid_key_usage_exception()
    });
    let error = anyhow::Error::new(error).context("KMS ciphertext authentication failed");
    if rejected {
        CiphertextRejected::tag(error)
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kms_requests_carry_the_same_context_and_pin_the_key() {
        let config = aws_sdk_kms::config::Builder::new()
            .behavior_version_latest()
            .region(aws_sdk_kms::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_kms::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .build();
        let provider = AwsKmsEncryptionProvider {
            client: KmsClient::from_conf(config),
            key_id: "alias/rise".into(),
        };
        let context = EncryptionContext::extension(
            uuid::Uuid::new_v4(),
            "oauth",
            "login",
            "/spec/client_secret_encrypted",
        )
        .with_binding("destination".into());
        let encrypt = provider
            .encrypt_request("secret", &context)
            .as_input()
            .clone()
            .build()
            .unwrap();
        let decrypt = provider
            .decrypt_request(vec![1], Some(kms_context(&context)))
            .as_input()
            .clone()
            .build()
            .unwrap();
        assert_eq!(encrypt.encryption_context(), decrypt.encryption_context());
        assert_eq!(encrypt.encryption_context(), Some(&kms_context(&context)));
        assert_eq!(decrypt.key_id(), Some("alias/rise"));
        assert_ne!(
            kms_context(&context),
            kms_context(&context.with_slot("/status/secret"))
        );
        assert!(provider
            .decrypt_request(vec![1], None)
            .as_input()
            .get_encryption_context()
            .is_none());
    }

    #[test]
    fn only_kms_verdicts_on_the_ciphertext_are_tagged_rejected() {
        use aws_sdk_kms::config::http::HttpResponse;
        use aws_sdk_kms::error::SdkError;
        use aws_sdk_kms::operation::decrypt::DecryptError;
        use aws_sdk_kms::types::error::{
            DisabledException, IncorrectKeyException, InvalidCiphertextException,
            InvalidKeyUsageException, KeyUnavailableException, KmsInternalException,
            KmsInvalidStateException, NotFoundException,
        };
        use rise_backend_core::encryption::is_ciphertext_rejected;

        fn service(error: DecryptError) -> SdkError<DecryptError, HttpResponse> {
            SdkError::service_error(error, HttpResponse::new(400.try_into().unwrap(), "".into()))
        }

        let rejected = [
            DecryptError::InvalidCiphertextException(InvalidCiphertextException::builder().build()),
            DecryptError::IncorrectKeyException(IncorrectKeyException::builder().build()),
            DecryptError::InvalidKeyUsageException(InvalidKeyUsageException::builder().build()),
        ];
        for error in rejected {
            let error = decrypt_error(service(error));
            assert!(is_ciphertext_rejected(&error), "{error:?}");
            assert_eq!(error.to_string(), "KMS ciphertext authentication failed");
            assert!(error
                .chain()
                .any(|c| c.is::<SdkError<DecryptError, HttpResponse>>()));
        }

        let transient = [
            service(DecryptError::KmsInternalException(
                KmsInternalException::builder().build(),
            )),
            service(DecryptError::KeyUnavailableException(
                KeyUnavailableException::builder().build(),
            )),
            service(DecryptError::DisabledException(
                DisabledException::builder().build(),
            )),
            service(DecryptError::KmsInvalidStateException(
                KmsInvalidStateException::builder().build(),
            )),
            service(DecryptError::NotFoundException(
                NotFoundException::builder().build(),
            )),
            service(DecryptError::unhandled("AccessDeniedException")),
            SdkError::timeout_error("timed out"),
            SdkError::construction_failure("no credentials"),
        ];
        for error in transient {
            let error = decrypt_error(error);
            assert!(!is_ciphertext_rejected(&error), "{error:?}");
            assert_eq!(error.to_string(), "KMS ciphertext authentication failed");
        }
    }
}
