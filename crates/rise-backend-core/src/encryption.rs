//! Authenticated credential scopes shared by encryption producers and consumers.

use std::fmt;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENVELOPE_PREFIX: &str = "rise:enc:v2:";

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

/// Malformed envelopes are tagged [`CiphertextRejected`].
pub fn decode_envelope(envelope: &str) -> Result<Vec<u8>> {
    let Some(value) = envelope.strip_prefix(ENVELOPE_PREFIX) else {
        return Err(CiphertextRejected::tag(anyhow!(
            "A context-bound rise:enc:v2 envelope is required"
        )));
    };
    URL_SAFE_NO_PAD
        .decode(value)
        .context("Invalid v2 ciphertext encoding")
        .map_err(CiphertextRejected::tag)
}

/// Marks a decryption error as a definite verdict on the ciphertext: it is malformed, was
/// tampered with, is bound to another [`EncryptionContext`], or was encrypted under a key the
/// provider does not hold. Retrying cannot help.
///
/// Providers tag only these errors. Everything else (an unreachable key service, a timeout,
/// throttling, missing permissions) stays untagged, so a caller that must fail safe treats it
/// as transient. The tag is transparent: a tagged error displays, debug-prints and chains
/// exactly like the error it wraps.
pub struct CiphertextRejected(anyhow::Error);

impl CiphertextRejected {
    /// Tags `error` as a rejected ciphertext without changing its message or cause chain.
    pub fn tag(error: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self(error.into()))
    }
}

impl fmt::Display for CiphertextRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the wrapped error's own message: `anyhow` renders the rest of the chain
        // through `source`.
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for CiphertextRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl std::error::Error for CiphertextRejected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Whether `error`, or any error it wraps, is tagged [`CiphertextRejected`].
pub fn is_ciphertext_rejected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<CiphertextRejected>())
}

/// Escape a name for a JSON pointer segment.
pub fn pointer_segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_envelopes_are_rejected_with_unchanged_messages() {
        let missing_prefix = decode_envelope("rise:enc:v3:AA").unwrap_err();
        assert!(is_ciphertext_rejected(&missing_prefix));
        assert_eq!(
            format!("{missing_prefix:#}"),
            "A context-bound rise:enc:v2 envelope is required"
        );

        let bad_encoding = decode_envelope("rise:enc:v2:not*base64").unwrap_err();
        assert!(is_ciphertext_rejected(&bad_encoding));
        let untagged = URL_SAFE_NO_PAD
            .decode("not*base64")
            .context("Invalid v2 ciphertext encoding")
            .unwrap_err();
        assert_eq!(format!("{bad_encoding}"), format!("{untagged}"));
        assert_eq!(format!("{bad_encoding:#}"), format!("{untagged:#}"));
        assert_eq!(
            bad_encoding
                .chain()
                .map(|c| c.to_string())
                .collect::<Vec<_>>(),
            untagged.chain().map(|c| c.to_string()).collect::<Vec<_>>()
        );
        assert!(bad_encoding.chain().any(|c| c.is::<base64::DecodeError>()));
    }

    #[test]
    fn the_tag_survives_added_context_and_is_absent_otherwise() {
        let tagged = CiphertextRejected::tag(anyhow!("authentication failed"));
        let wrapped = Err::<(), _>(tagged)
            .context("decrypting the client secret")
            .unwrap_err();
        assert!(is_ciphertext_rejected(&wrapped));
        assert_eq!(
            format!("{wrapped:#}"),
            "decrypting the client secret: authentication failed"
        );
        assert!(!is_ciphertext_rejected(&anyhow!("KMS unreachable")));
        assert!(!is_ciphertext_rejected(
            &Err::<(), _>(anyhow!("timeout"))
                .context("decrypting")
                .unwrap_err()
        ));
        assert!(decode_envelope(&encode_envelope(b"bytes")).is_ok());
    }
}
