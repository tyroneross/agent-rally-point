// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Trust classification for Rally events.
//!
//! This crate keeps verification separate from the protocol crate so the
//! portable event boundary stays usable even when a caller only wants to read or
//! merge JSONL records. Policy is local and optional: callers may classify raw
//! signature validity, or add a small policy to decide whether a valid key is
//! trusted for a tool/kind pair.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rally_protocol::{
    CANONICALIZATION_VERSION, ProtocolError, SignatureEnvelope, canonical_event_bytes, event_value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustStatus {
    Unsigned,
    Valid,
    Trusted,
    ValidUntrusted,
    UnknownKey,
    Invalid,
    Unsupported,
}

impl fmt::Display for TrustStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Unsigned => "unsigned",
            Self::Valid => "valid",
            Self::Trusted => "trusted",
            Self::ValidUntrusted => "valid-untrusted",
            Self::UnknownKey => "unknown-key",
            Self::Invalid => "invalid",
            Self::Unsupported => "unsupported",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Classification {
    pub status: TrustStatus,
    pub key_id: Option<String>,
}

impl Classification {
    pub fn new(status: TrustStatus, key_id: Option<String>) -> Self {
        Self { status, key_id }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PublicKeyStore {
    keys: HashMap<String, Vec<u8>>,
}

impl PublicKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_base64(
        &mut self,
        key_id: impl Into<String>,
        public_key: &str,
    ) -> Result<(), TrustError> {
        let bytes = STANDARD
            .decode(public_key)
            .map_err(|_| TrustError::InvalidKeyMaterial)?;
        self.insert_bytes(key_id, bytes);
        Ok(())
    }

    pub fn insert_bytes(&mut self, key_id: impl Into<String>, public_key: impl Into<Vec<u8>>) {
        self.keys.insert(key_id.into(), public_key.into());
    }

    fn get(&self, key_id: &str) -> Option<&[u8]> {
        self.keys.get(key_id).map(Vec::as_slice)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrustPolicy {
    entries: HashMap<String, KeyPolicy>,
}

impl TrustPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust_key_for(
        mut self,
        key_id: impl Into<String>,
        tools: impl IntoIterator<Item = impl Into<String>>,
        kinds: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.entries.insert(
            key_id.into(),
            KeyPolicy {
                trusted_tools: tools.into_iter().map(Into::into).collect(),
                allowed_kinds: kinds.into_iter().map(Into::into).collect(),
            },
        );
        self
    }

    fn permits(&self, key_id: &str, event: &Value) -> bool {
        let Some(policy) = self.entries.get(key_id) else {
            return false;
        };
        let tool = event.get("tool").and_then(Value::as_str);
        let kind = event.get("kind").and_then(Value::as_str);
        tool.is_some_and(|value| policy.trusted_tools.contains(value))
            && kind.is_some_and(|value| policy.allowed_kinds.contains(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KeyPolicy {
    trusted_tools: HashSet<String>,
    allowed_kinds: HashSet<String>,
}

#[derive(Debug)]
pub enum TrustError {
    Protocol(ProtocolError),
    InvalidKeyMaterial,
    MalformedSignature,
    Io(std::io::Error),
    Toml(toml::de::Error),
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(err) => write!(f, "{err}"),
            Self::InvalidKeyMaterial => f.write_str("invalid public key material"),
            Self::MalformedSignature => f.write_str("malformed signature envelope"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Toml(err) => write!(f, "TOML error: {err}"),
        }
    }
}

impl std::error::Error for TrustError {}

impl From<ProtocolError> for TrustError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

impl From<std::io::Error> for TrustError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<toml::de::Error> for TrustError {
    fn from(value: toml::de::Error) -> Self {
        Self::Toml(value)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrustContext {
    pub keys: PublicKeyStore,
    pub policy: TrustPolicy,
}

#[derive(Debug, Deserialize)]
struct TrustFile {
    #[serde(default)]
    keys: Vec<TrustFileKey>,
}

#[derive(Debug, Deserialize)]
struct TrustFileKey {
    key_id: String,
    public_key: String,
    #[serde(default)]
    trusted_tools: Vec<String>,
    #[serde(default)]
    allowed_kinds: Vec<String>,
}

pub fn load_trust_file(path: impl AsRef<Path>) -> Result<TrustContext, TrustError> {
    let text = fs::read_to_string(path)?;
    let trust_file: TrustFile = toml::from_str(&text)?;
    let mut context = TrustContext::default();
    for key in trust_file.keys {
        context
            .keys
            .insert_base64(key.key_id.clone(), &key.public_key)?;
        context.policy =
            context
                .policy
                .trust_key_for(key.key_id, key.trusted_tools, key.allowed_kinds);
    }
    Ok(context)
}

pub fn classify(record: &Value, keys: &PublicKeyStore) -> Result<Classification, TrustError> {
    classify_with_policy(record, keys, None)
}

pub fn classify_with_policy(
    record: &Value,
    keys: &PublicKeyStore,
    policy: Option<&TrustPolicy>,
) -> Result<Classification, TrustError> {
    let signature = match signature_envelope(record) {
        Ok(Some(signature)) => signature,
        Ok(None) => return Ok(Classification::new(TrustStatus::Unsigned, None)),
        Err(TrustError::MalformedSignature) => {
            return Ok(Classification::new(TrustStatus::Invalid, None));
        }
        Err(err) => return Err(err),
    };

    let key_id = signature.key_id.clone();
    if !signature.algorithm.eq_ignore_ascii_case("ed25519")
        || signature.version != "rally-signature-v1"
        || signature
            .canonicalization
            .as_deref()
            .is_some_and(|value| value != CANONICALIZATION_VERSION)
    {
        return Ok(Classification::new(TrustStatus::Unsupported, Some(key_id)));
    }

    let Some(public_key) = keys.get(&key_id) else {
        return Ok(Classification::new(TrustStatus::UnknownKey, Some(key_id)));
    };

    let valid = verify_ed25519(record, public_key, &signature)?;
    if !valid {
        return Ok(Classification::new(TrustStatus::Invalid, Some(key_id)));
    }

    let status = match policy {
        Some(policy) if policy.permits(&key_id, &event_value(record)?) => TrustStatus::Trusted,
        Some(_) => TrustStatus::ValidUntrusted,
        None => TrustStatus::Valid,
    };
    Ok(Classification::new(status, Some(key_id)))
}

fn signature_envelope(record: &Value) -> Result<Option<SignatureEnvelope>, TrustError> {
    let event = event_value(record)?;
    let object = event.as_object().ok_or(ProtocolError::ExpectedObject)?;
    let Some(signature) = object.get("signature") else {
        return Ok(None);
    };
    serde_json::from_value(signature.clone())
        .map(Some)
        .map_err(|_| TrustError::MalformedSignature)
}

fn verify_ed25519(
    record: &Value,
    public_key: &[u8],
    signature: &SignatureEnvelope,
) -> Result<bool, TrustError> {
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| TrustError::InvalidKeyMaterial)?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key).map_err(|_| TrustError::InvalidKeyMaterial)?;

    let signature_bytes = match STANDARD.decode(&signature.signature) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    let signature_bytes: [u8; 64] = match signature_bytes.try_into() {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    let signature = Signature::from_bytes(&signature_bytes);
    Ok(verifying_key
        .verify(&canonical_event_bytes(record)?, &signature)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rally_protocol::{CANONICALIZATION_VERSION, canonical_event_bytes};
    use serde_json::json;

    #[test]
    fn unsigned_records_classify_as_unsigned() {
        let record = json!({
            "id": "evt_11111111111111111111111111111111",
            "kind": "handoff",
            "type": "agent-rally.handoff.created.v1",
            "tool": "codex",
            "payload": {"subject": "review"}
        });

        let classification = classify(&record, &PublicKeyStore::new()).unwrap();
        assert_eq!(classification.status, TrustStatus::Unsigned);
        assert_eq!(classification.key_id, None);
    }

    #[test]
    fn malformed_signature_classifies_as_invalid() {
        let record = json!({
            "id": "evt_11111111111111111111111111111111",
            "kind": "handoff",
            "type": "agent-rally.handoff.created.v1",
            "tool": "codex",
            "payload": {"subject": "review"},
            "signature": {"algorithm": "ed25519"}
        });

        let classification = classify(&record, &PublicKeyStore::new()).unwrap();
        assert_eq!(classification.status, TrustStatus::Invalid);
        assert_eq!(classification.key_id, None);
    }

    #[test]
    fn valid_signature_classifies_as_valid_or_trusted() {
        let mut record = json!({
            "id": "evt_11111111111111111111111111111111",
            "kind": "handoff",
            "type": "agent-rally.handoff.created.v1",
            "tool": "codex",
            "payload": {"subject": "review"}
        });
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let signature = signing_key.sign(&canonical_event_bytes(&record).unwrap());
        record.as_object_mut().unwrap().insert(
            "signature".into(),
            json!({
                "version": "rally-signature-v1",
                "algorithm": "ed25519",
                "key_id": "key_codex_test",
                "signed_at": "2026-05-26T18:00:00.000Z",
                "signature": STANDARD.encode(signature.to_bytes()),
                "canonicalization": CANONICALIZATION_VERSION
            }),
        );

        let mut keys = PublicKeyStore::new();
        keys.insert_bytes("key_codex_test", signing_key.verifying_key().to_bytes());
        let policy = TrustPolicy::new().trust_key_for("key_codex_test", ["codex"], ["handoff"]);

        assert_eq!(classify(&record, &keys).unwrap().status, TrustStatus::Valid);
        assert_eq!(
            classify_with_policy(&record, &keys, Some(&policy))
                .unwrap()
                .status,
            TrustStatus::Trusted
        );
    }

    #[test]
    fn load_trust_file_builds_key_store_and_policy() {
        let signing_key = SigningKey::from_bytes(&[9; 32]);
        let path = std::env::temp_dir().join(format!(
            "rally-trust-{}.toml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            format!(
                r#"
[[keys]]
key_id = "key_codex_test"
public_key = "{}"
trusted_tools = ["codex"]
allowed_kinds = ["handoff"]
"#,
                STANDARD.encode(signing_key.verifying_key().to_bytes())
            ),
        )
        .unwrap();

        let context = load_trust_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();

        let mut record = json!({
            "id": "evt_11111111111111111111111111111111",
            "kind": "handoff",
            "type": "agent-rally.handoff.created.v1",
            "tool": "codex",
            "payload": {"subject": "review"}
        });
        let signature = signing_key.sign(&canonical_event_bytes(&record).unwrap());
        record.as_object_mut().unwrap().insert(
            "signature".into(),
            json!({
                "version": "rally-signature-v1",
                "algorithm": "ed25519",
                "key_id": "key_codex_test",
                "signed_at": "2026-05-26T18:00:00.000Z",
                "signature": STANDARD.encode(signature.to_bytes()),
                "canonicalization": CANONICALIZATION_VERSION
            }),
        );

        assert_eq!(
            classify_with_policy(&record, &context.keys, Some(&context.policy))
                .unwrap()
                .status,
            TrustStatus::Trusted
        );
    }
}
