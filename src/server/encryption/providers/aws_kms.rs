use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::DataKeySpec;
use aws_sdk_kms::Client as KmsClient;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::server::encryption::EncryptionProvider;

/// Magic prefix to identify envelope-encrypted data (vs legacy direct KMS encryption).
/// "ENVL" in ASCII bytes.
const ENVELOPE_PREFIX: &[u8] = b"ENVL";

/// AWS KMS encryption provider
///
/// Uses envelope encryption: a data encryption key (DEK) is generated via KMS,
/// the plaintext is encrypted locally with AES-256-GCM using the DEK, and the
/// KMS-encrypted DEK is stored alongside the ciphertext. This avoids the KMS
/// 4096-byte plaintext limit.
///
/// Legacy ciphertexts (direct KMS encryption) are detected on decrypt by the
/// absence of the envelope prefix and handled transparently.
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
    async fn encrypt(&self, plaintext: &str) -> Result<String> {
        // Generate a data encryption key via KMS
        let response = self
            .client
            .generate_data_key()
            .key_id(&self.key_id)
            .key_spec(DataKeySpec::Aes256)
            .send()
            .await
            .with_context(|| {
                format!(
                    "KMS GenerateDataKey failed for key '{}'. Common causes: \
                     1) Invalid key ARN/ID, 2) No AWS credentials available, \
                     3) Insufficient IAM permissions (kms:GenerateDataKey), \
                     4) Key is disabled or pending deletion",
                    self.key_id
                )
            })?;

        let plaintext_key = response
            .plaintext()
            .context("No plaintext key in GenerateDataKey response")?;
        let encrypted_key = response
            .ciphertext_blob()
            .context("No ciphertext blob in GenerateDataKey response")?;
        let encrypted_key_bytes = encrypted_key.as_ref();

        // Encrypt the data locally with AES-256-GCM
        let cipher = Aes256Gcm::new_from_slice(plaintext_key.as_ref())
            .context("Failed to create AES-256-GCM cipher from data key")?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("AES-GCM encryption failed: {}", e))?;

        // Pack: prefix(4) + encrypted_key_len(2 BE) + encrypted_key + nonce(12) + ciphertext
        let ek_len = encrypted_key_bytes.len() as u16;
        let mut combined = Vec::with_capacity(
            ENVELOPE_PREFIX.len() + 2 + encrypted_key_bytes.len() + 12 + ciphertext.len(),
        );
        combined.extend_from_slice(ENVELOPE_PREFIX);
        combined.extend_from_slice(&ek_len.to_be_bytes());
        combined.extend_from_slice(encrypted_key_bytes);
        combined.extend_from_slice(&nonce);
        combined.extend_from_slice(&ciphertext);

        Ok(BASE64.encode(&combined))
    }

    async fn decrypt(&self, ciphertext_base64: &str) -> Result<String> {
        let combined = BASE64
            .decode(ciphertext_base64)
            .context("Failed to decode ciphertext from base64")?;

        // Detect format: envelope (prefixed) vs legacy (direct KMS)
        if combined.starts_with(ENVELOPE_PREFIX) {
            self.decrypt_envelope(&combined[ENVELOPE_PREFIX.len()..])
                .await
        } else {
            self.decrypt_legacy(&combined).await
        }
    }
}

impl AwsKmsEncryptionProvider {
    /// Decrypt envelope-encrypted data (new format).
    async fn decrypt_envelope(&self, data: &[u8]) -> Result<String> {
        if data.len() < 2 {
            bail!("Invalid envelope ciphertext: too short for key length");
        }

        // Read encrypted key length
        let ek_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        let data = &data[2..];

        if data.len() < ek_len + 12 {
            bail!("Invalid envelope ciphertext: too short for key + nonce");
        }

        let (encrypted_key, rest) = data.split_at(ek_len);
        let (nonce_bytes, ciphertext) = rest.split_at(12);

        // Decrypt the DEK via KMS
        let response = self
            .client
            .decrypt()
            .ciphertext_blob(Blob::new(encrypted_key.to_vec()))
            .send()
            .await
            .with_context(|| {
                format!(
                    "KMS decryption of data key failed for key '{}'. Common causes: \
                     1) No AWS credentials available, 2) Insufficient IAM permissions (kms:Decrypt), \
                     3) Key is disabled or pending deletion",
                    self.key_id
                )
            })?;

        let plaintext_key = response
            .plaintext()
            .context("No plaintext in KMS decrypt response")?;

        // Decrypt the data locally with AES-256-GCM
        let cipher = Aes256Gcm::new_from_slice(plaintext_key.as_ref())
            .context("Failed to create AES-256-GCM cipher from decrypted data key")?;
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext_bytes = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("AES-GCM decryption failed: {}", e))?;

        String::from_utf8(plaintext_bytes).context("Decrypted data is not valid UTF-8")
    }

    /// Decrypt legacy direct-KMS-encrypted data (backward compatibility).
    async fn decrypt_legacy(&self, ciphertext_bytes: &[u8]) -> Result<String> {
        let response = self
            .client
            .decrypt()
            .ciphertext_blob(Blob::new(ciphertext_bytes.to_vec()))
            .send()
            .await
            .with_context(|| {
                format!(
                    "KMS decryption failed for key '{}'. Common causes: \
                     1) No AWS credentials available, 2) Insufficient IAM permissions (kms:Decrypt), \
                     3) Key is disabled or pending deletion, 4) Ciphertext was encrypted with a different key",
                    self.key_id
                )
            })?;

        let plaintext_blob = response
            .plaintext()
            .context("No plaintext in KMS response")?;

        String::from_utf8(plaintext_blob.clone().into_inner())
            .context("Decrypted data is not valid UTF-8")
    }
}
