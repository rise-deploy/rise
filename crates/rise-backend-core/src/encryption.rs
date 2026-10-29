//! Authenticated credential scopes shared by encryption producers and consumers.

use anyhow::{bail, Context, Result};
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

pub fn decode_envelope(envelope: &str) -> Result<Vec<u8>> {
    let Some(value) = envelope.strip_prefix(ENVELOPE_PREFIX) else {
        bail!("A context-bound rise:enc:v2 envelope is required");
    };
    URL_SAFE_NO_PAD
        .decode(value)
        .context("Invalid v2 ciphertext encoding")
}

/// Escape a name for a JSON pointer segment.
pub fn pointer_segment(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}
