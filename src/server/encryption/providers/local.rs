use aes_gcm::{
    aead::{Aead, Generate, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::server::encryption::EncryptionProvider;

/// Local encryption provider using AES-256-GCM
/// Encrypts data using a symmetric key stored in backend configuration
pub struct LocalEncryptionProvider {
    cipher: Aes256Gcm,
}

impl LocalEncryptionProvider {
    /// Create a new local encryption provider from a base64-encoded key
    pub fn new(key_base64: &str) -> Result<Self> {
        let key_bytes = BASE64
            .decode(key_base64)
            .context("Failed to decode encryption key from base64")?;

        if key_bytes.len() != 32 {
            bail!(
                "Encryption key must be 32 bytes (256 bits) for AES-256-GCM, got {} bytes",
                key_bytes.len()
            );
        }

        let cipher =
            Aes256Gcm::new_from_slice(&key_bytes).context("Failed to create AES-256-GCM cipher")?;

        Ok(Self { cipher })
    }
}

#[async_trait]
impl EncryptionProvider for LocalEncryptionProvider {
    async fn encrypt(&self, plaintext: &str) -> Result<String> {
        // Generate random 12-byte nonce
        let nonce = Nonce::generate();

        // Encrypt the plaintext
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

        // Combine nonce + ciphertext + tag for storage
        let mut combined = nonce.to_vec();
        combined.extend_from_slice(&ciphertext);

        // Encode to base64 for storage in database
        Ok(BASE64.encode(&combined))
    }

    async fn decrypt(&self, ciphertext_base64: &str) -> Result<String> {
        // Decode from base64
        let combined = BASE64
            .decode(ciphertext_base64)
            .context("Failed to decode ciphertext from base64")?;

        // Extract nonce (first 12 bytes) and ciphertext (rest)
        if combined.len() < 12 {
            bail!("Invalid ciphertext: too short");
        }

        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::try_from(nonce_bytes).context("Invalid ciphertext: malformed nonce")?;

        // Decrypt
        let plaintext_bytes = self
            .cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

        // Convert to UTF-8 string
        let plaintext =
            String::from_utf8(plaintext_bytes).context("Decrypted data is not valid UTF-8")?;

        Ok(plaintext)
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
        let ciphertext = provider.encrypt(plaintext).await.unwrap();
        let decrypted = provider.decrypt(&ciphertext).await.unwrap();

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
        let ciphertext1 = provider.encrypt(plaintext).await.unwrap();
        let ciphertext2 = provider.encrypt(plaintext).await.unwrap();

        // Ciphertexts should be different (different nonces)
        assert_ne!(ciphertext1, ciphertext2);

        // But both should decrypt to the same plaintext
        assert_eq!(provider.decrypt(&ciphertext1).await.unwrap(), plaintext);
        assert_eq!(provider.decrypt(&ciphertext2).await.unwrap(), plaintext);
    }

    #[tokio::test]
    async fn test_invalid_key_length() {
        let short_key = BASE64.encode(b"tooshort");
        let result = LocalEncryptionProvider::new(&short_key);
        assert!(result.is_err());
    }
}
