use aes_gcm::{
    aead::{Aead, Generate, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use rise_backend_core::encryption::{decode_envelope, encode_envelope, CiphertextRejected};

/// Local encryption provider using AES-256-GCM
/// Encrypts data using a symmetric key stored in backend configuration
pub struct LocalEncryptionProvider {
    cipher: Aes256Gcm,
    migration_source_cipher: Option<Aes256Gcm>,
}

impl LocalEncryptionProvider {
    /// Create a new local encryption provider from a base64-encoded key
    pub fn new(key_base64: &str) -> Result<Self> {
        Ok(Self {
            cipher: Self::cipher_from_key(key_base64)?,
            migration_source_cipher: None,
        })
    }

    pub fn new_with_migration_source_key(
        key_base64: &str,
        source_key: Option<&str>,
    ) -> Result<Self> {
        let mut provider = Self::new(key_base64)?;
        provider.migration_source_cipher = source_key
            .map(Self::cipher_from_key)
            .transpose()
            .context("Invalid migration source key")?;
        Ok(provider)
    }

    fn cipher_from_key(key_base64: &str) -> Result<Aes256Gcm> {
        let key_bytes = BASE64
            .decode(key_base64)
            .context("Failed to decode encryption key from base64")?;
        if key_bytes.len() != 32 {
            bail!(
                "Encryption key must be 32 bytes (256 bits) for AES-256-GCM, got {} bytes",
                key_bytes.len()
            );
        }
        Aes256Gcm::new_from_slice(&key_bytes).context("Failed to create AES-256-GCM cipher")
    }
}

#[async_trait]
impl EncryptionProvider for LocalEncryptionProvider {
    async fn encrypt(&self, plaintext: &str, context: &EncryptionContext) -> Result<String> {
        // Generate random 12-byte nonce
        let nonce = Nonce::generate();

        // Encrypt the plaintext
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &context.authenticated_bytes(),
                },
            )
            .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

        // Combine nonce + ciphertext + tag for storage
        let mut combined = nonce.to_vec();
        combined.extend_from_slice(&ciphertext);

        // Encode to base64 for storage in database
        Ok(encode_envelope(&combined))
    }

    async fn decrypt(&self, envelope: &str, context: &EncryptionContext) -> Result<String> {
        self.decrypt_bytes(&decode_envelope(envelope)?, &context.authenticated_bytes())
    }

    async fn decrypt_legacy_for_migration(&self, ciphertext: &str) -> Result<String> {
        if ciphertext.starts_with("rise:") {
            bail!("Unsupported baseline ciphertext version");
        }
        Self::decrypt_with_cipher(
            self.migration_source_cipher
                .as_ref()
                .unwrap_or(&self.cipher),
            &BASE64
                .decode(ciphertext)
                .context("Invalid baseline encoding")?,
            &[],
        )
    }
}

impl LocalEncryptionProvider {
    #[cfg(test)]
    pub(crate) fn legacy_ciphertext_for_test(&self, plaintext: &str) -> String {
        let nonce = Nonce::generate();
        let ciphertext = self.cipher.encrypt(&nonce, plaintext.as_bytes()).unwrap();
        let mut combined = nonce.to_vec();
        combined.extend(ciphertext);
        BASE64.encode(combined)
    }

    fn decrypt_bytes(&self, combined: &[u8], aad: &[u8]) -> Result<String> {
        Self::decrypt_with_cipher(&self.cipher, combined, aad)
    }

    fn decrypt_with_cipher(cipher: &Aes256Gcm, combined: &[u8], aad: &[u8]) -> Result<String> {
        if combined.len() < 28 {
            return Err(CiphertextRejected::tag(anyhow::anyhow!(
                "Invalid ciphertext: too short"
            )));
        }
        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::try_from(nonce_bytes)
            .context("Invalid ciphertext nonce")
            .map_err(CiphertextRejected::tag)?;
        let plaintext = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                CiphertextRejected::tag(anyhow::anyhow!("Ciphertext authentication failed"))
            })?;
        String::from_utf8(plaintext)
            .context("Decrypted data is not valid UTF-8")
            .map_err(CiphertextRejected::tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_encrypt_decrypt_roundtrip() {
        // Generate a random 32-byte key for testing
        use rand::Rng;
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let key_base64 = BASE64.encode(key);

        let provider = LocalEncryptionProvider::new(&key_base64).unwrap();

        let plaintext = "my secret password";
        let ciphertext = provider
            .encrypt(plaintext, &EncryptionContext::internal("test"))
            .await
            .unwrap();
        let decrypted = provider
            .decrypt(&ciphertext, &EncryptionContext::internal("test"))
            .await
            .unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[tokio::test]
    async fn test_different_nonces() {
        // Encrypting the same plaintext twice should produce different ciphertexts
        use rand::Rng;
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let key_base64 = BASE64.encode(key);
        let provider = LocalEncryptionProvider::new(&key_base64).unwrap();

        let plaintext = "same message";
        let ciphertext1 = provider
            .encrypt(plaintext, &EncryptionContext::internal("test"))
            .await
            .unwrap();
        let ciphertext2 = provider
            .encrypt(plaintext, &EncryptionContext::internal("test"))
            .await
            .unwrap();

        // Ciphertexts should be different (different nonces)
        assert_ne!(ciphertext1, ciphertext2);

        // But both should decrypt to the same plaintext
        assert_eq!(
            provider
                .decrypt(&ciphertext1, &EncryptionContext::internal("test"))
                .await
                .unwrap(),
            plaintext
        );
        assert_eq!(
            provider
                .decrypt(&ciphertext2, &EncryptionContext::internal("test"))
                .await
                .unwrap(),
            plaintext
        );
    }

    #[tokio::test]
    async fn test_invalid_key_length() {
        let short_key = BASE64.encode(b"tooshort");
        let result = LocalEncryptionProvider::new(&short_key);
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn scope_slot_and_destination_are_authenticated() {
        use uuid::Uuid;
        let provider = LocalEncryptionProvider::new(&BASE64.encode([7u8; 32])).unwrap();
        let project = Uuid::new_v4();
        let context = EncryptionContext::extension(
            project,
            "oauth",
            "login",
            "/spec/client_secret_encrypted",
        )
        .with_binding("client-and-endpoint".into());
        let ciphertext = provider.encrypt("credential", &context).await.unwrap();
        assert!(ciphertext.starts_with("rise:enc:v2:"));
        for wrong in [
            EncryptionContext::extension(Uuid::new_v4(), "oauth", "login", &context.slot)
                .with_binding("client-and-endpoint".into()),
            EncryptionContext::extension(project, "oauth", "other", &context.slot)
                .with_binding("client-and-endpoint".into()),
            EncryptionContext::extension(project, "other-kind", "login", &context.slot)
                .with_binding("client-and-endpoint".into()),
            context.with_slot("/status/oauth_client_secret_encrypted"),
            context.clone().with_binding("different-endpoint".into()),
            EncryptionContext::project_env(project, None, "TOKEN"),
        ] {
            assert!(provider.decrypt(&ciphertext, &wrong).await.is_err());
        }
        // Recreating the same extension name in the same project is an accepted scope.
        assert_eq!(
            provider
                .decrypt(&ciphertext, &context.clone())
                .await
                .unwrap(),
            "credential"
        );
    }

    #[tokio::test]
    async fn ordinary_decryption_never_accepts_unbound_or_unknown_versions() {
        let provider = LocalEncryptionProvider::new(&BASE64.encode([7u8; 32])).unwrap();
        let context = EncryptionContext::internal("test");
        let legacy = provider.legacy_ciphertext_for_test("credential");
        assert!(provider.decrypt(&legacy, &context).await.is_err());
        assert_eq!(
            provider
                .decrypt_legacy_for_migration(&legacy)
                .await
                .unwrap(),
            "credential"
        );
        assert!(provider.decrypt("rise:enc:v3:AA", &context).await.is_err());
        assert!(provider
            .decrypt_legacy_for_migration("rise:enc:v3:AA")
            .await
            .is_err());
        let bound = provider.encrypt("credential", &context).await.unwrap();
        let stripped = BASE64.encode(decode_envelope(&bound).unwrap());
        assert!(provider
            .decrypt_legacy_for_migration(&stripped)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rejected_ciphertexts_are_tagged_with_unchanged_messages() {
        use rise_backend_core::encryption::is_ciphertext_rejected;
        let provider = LocalEncryptionProvider::new(&BASE64.encode([7u8; 32])).unwrap();
        let other_key = LocalEncryptionProvider::new(&BASE64.encode([8u8; 32])).unwrap();
        let context = EncryptionContext::internal("test");
        let ciphertext = provider.encrypt("credential", &context).await.unwrap();

        let cases = [
            (
                provider
                    .decrypt(&ciphertext, &context.with_slot("other"))
                    .await,
                "Ciphertext authentication failed",
            ),
            (
                other_key.decrypt(&ciphertext, &context).await,
                "Ciphertext authentication failed",
            ),
            (
                provider
                    .decrypt(&encode_envelope(&[0u8; 27]), &context)
                    .await,
                "Invalid ciphertext: too short",
            ),
            (
                provider.decrypt("rise:enc:v2:***", &context).await,
                "Invalid v2 ciphertext encoding",
            ),
            (
                provider.decrypt("plain", &context).await,
                "A context-bound rise:enc:v2 envelope is required",
            ),
        ];
        for (result, message) in cases {
            let error = result.unwrap_err();
            assert!(is_ciphertext_rejected(&error), "{error:#}");
            assert_eq!(error.to_string(), message);
        }

        let mut combined = decode_envelope(&ciphertext).unwrap();
        let nonce = Nonce::try_from(&combined[..12]).unwrap();
        let invalid_utf8 = provider
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &[0xff, 0xfe],
                    aad: &context.authenticated_bytes(),
                },
            )
            .unwrap();
        combined.truncate(12);
        combined.extend(invalid_utf8);
        let error = provider
            .decrypt(&encode_envelope(&combined), &context)
            .await
            .unwrap_err();
        assert!(is_ciphertext_rejected(&error));
        assert_eq!(error.to_string(), "Decrypted data is not valid UTF-8");
        assert!(error.chain().any(|c| c.is::<std::string::FromUtf8Error>()));
    }
}
