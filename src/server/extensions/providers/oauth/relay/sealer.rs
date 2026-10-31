//! `oauthrelay_core::Sealer` that seals relay envelopes with a provider-wrapped data key.

use std::sync::Arc;

use oauthrelay_core::{SealError, Sealer};

use crate::server::encryption::EncryptionProvider;

/// Seals and opens relay envelopes using Rise's configured encryption provider.
pub struct RiseSealer {
    provider: Arc<dyn EncryptionProvider>,
}

impl RiseSealer {
    pub fn new(provider: Arc<dyn EncryptionProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait::async_trait]
impl Sealer for RiseSealer {
    async fn seal(&self, _plaintext: &[u8], _associated_data: &[u8]) -> Result<String, SealError> {
        Err(SealError::Unavailable)
    }

    async fn unseal(&self, _envelope: &str, _associated_data: &[u8]) -> Result<Vec<u8>, SealError> {
        Err(SealError::Unavailable)
    }
}
