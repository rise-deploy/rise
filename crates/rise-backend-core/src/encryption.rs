//! Authenticated credential scopes shared by encryption producers and consumers.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENVELOPE_PREFIX: &str = "rise:enc:v2:";

/// Marks a decryption failure caused by the ciphertext itself: it is malformed,
/// was tampered with, or does not authenticate for the expected key or context.
///
/// Attached as context to the error a provider returns; every other error from
/// a provider (throttling, network, timeouts, missing permissions) says nothing
/// about the ciphertext and is worth retrying. Test with
/// [`is_ciphertext_rejection`].
#[derive(Debug)]
pub struct CiphertextRejected;

impl std::fmt::Display for CiphertextRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ciphertext rejected")
    }
}

impl std::error::Error for CiphertextRejected {}

/// Whether `error` says the ciphertext is bad, as opposed to the provider being
/// unable to answer.
pub fn is_ciphertext_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CiphertextRejected>().is_some()
}

/// Consumers construct the expected context from trusted resource identity.
/// An envelope never supplies its own authority or intended use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptionContext {
    pub scope: Vec<String>,
    pub slot: String,
    pub binding: Option<String>,
}

impl EncryptionContext {
    pub fn extension(project: Uuid, kind: &str, name: &str, slot: &str) -> Self {
        Self {
            scope: vec![
                "project".into(),
                project.to_string(),
                "extension".into(),
                kind.into(),
                name.into(),
            ],
            slot: slot.into(),
            binding: None,
        }
    }

    /// Like [`Self::extension`], additionally bound to one incarnation of the
    /// extension: `created_at_micros` is the creation time of its row in unix
    /// microseconds. Deleting an extension and creating one of the same name
    /// yields a different incarnation, so a value sealed under this context
    /// does not decrypt for the new one.
    pub fn extension_instance(
        project: Uuid,
        kind: &str,
        name: &str,
        created_at_micros: i64,
        slot: &str,
    ) -> Self {
        let mut context = Self::extension(project, kind, name, slot);
        context.scope.push("created-at-micros".into());
        context.scope.push(created_at_micros.to_string());
        context
    }

    pub fn project_env(project: Uuid, environment: Option<Uuid>, key: &str) -> Self {
        Self {
            scope: vec![
                "project".into(),
                project.to_string(),
                "environment".into(),
                environment
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "global".into()),
            ],
            slot: key.into(),
            binding: None,
        }
    }

    pub fn deployment_env(deployment: Uuid, key: &str) -> Self {
        Self {
            scope: vec!["deployment".into(), deployment.to_string()],
            slot: key.into(),
            binding: None,
        }
    }

    pub fn internal(slot: &str) -> Self {
        Self {
            scope: vec!["rise-internal".into()],
            slot: slot.into(),
            binding: None,
        }
    }

    pub fn with_slot(&self, slot: &str) -> Self {
        Self {
            slot: slot.into(),
            ..self.clone()
        }
    }

    pub fn with_binding(mut self, binding: String) -> Self {
        self.binding = Some(binding);
        self
    }

    pub fn authenticated_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&("rise-encryption-v2", self))
            .expect("context contains only JSON strings")
    }
}

pub fn encode_envelope(ciphertext: &[u8]) -> String {
    format!("{ENVELOPE_PREFIX}{}", URL_SAFE_NO_PAD.encode(ciphertext))
}

pub fn decode_envelope(envelope: &str) -> Result<Vec<u8>> {
    let Some(value) = envelope.strip_prefix(ENVELOPE_PREFIX) else {
        return Err(
            anyhow::anyhow!("A context-bound rise:enc:v2 envelope is required")
                .context(CiphertextRejected),
        );
    };
    URL_SAFE_NO_PAD
        .decode(value)
        .context("Invalid v2 ciphertext encoding")
        .context(CiphertextRejected)
}

/// Escape a name for a JSON pointer segment.
pub fn pointer_segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_envelopes_are_ciphertext_rejections() {
        for envelope in ["", "plain", "rise:enc:v1:abc", "rise:enc:v2:%%%"] {
            let error = decode_envelope(envelope).unwrap_err();
            assert!(is_ciphertext_rejection(&error), "{envelope}");
        }
        let envelope = encode_envelope(b"cipher");
        assert_eq!(decode_envelope(&envelope).unwrap(), b"cipher");
        assert!(!is_ciphertext_rejection(&anyhow::anyhow!("throttled")));
    }

    #[test]
    fn extension_instance_contexts_differ_by_incarnation_and_from_the_plain_context() {
        let project = Uuid::from_u128(1);
        let plain = EncryptionContext::extension(project, "oauth", "login", "/slot");
        let a = EncryptionContext::extension_instance(project, "oauth", "login", 10, "/slot");
        let b = EncryptionContext::extension_instance(project, "oauth", "login", 11, "/slot");
        assert_ne!(a.authenticated_bytes(), b.authenticated_bytes());
        assert_ne!(a.authenticated_bytes(), plain.authenticated_bytes());
        assert_eq!(
            a.authenticated_bytes(),
            EncryptionContext::extension_instance(project, "oauth", "login", 10, "/slot")
                .authenticated_bytes()
        );
        // The plain context is unchanged, so values sealed under it keep decrypting.
        assert_eq!(plain.scope.len(), 5);
    }
}
