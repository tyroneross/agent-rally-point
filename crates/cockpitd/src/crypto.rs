// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Cryptographic primitives for the multi-user / zero-knowledge relay path (§8).
//!
//! All primitives use dryoc (pure-Rust libsodium API):
//! - **Identity**    — Ed25519 signing keypair derived from a 32-byte seed.
//! - **Challenge-response auth** — server issues random 32-byte challenge;
//!   client signs it; server verifies with public key.
//! - **Per-session data keys** — random 32-byte key + XSalsa20-Poly1305
//!   secretbox (libsodium `crypto_secretbox`) for seal/open.
//! - **Key wrapping** — X25519 sealed box (`crypto_box_seal`) wraps a data key
//!   for a recipient public key.  The server stores only the wrapped form —
//!   it never holds a bare data key.
//!
//! ### Data-residency invariant (§3)
//! The relay (and any persisted state) holds only ciphertext + wrapped keys.
//! It NEVER holds a bare data key.  `WrappedKey` is the transport form; only
//! the holder of the corresponding `RecipientKeypair` secret key can recover
//! the inner data key via `unwrap_key`.

use anyhow::{Result, anyhow};
use dryoc::{
    classic::{
        crypto_secretbox::{
            Key as SbKey, Nonce as SbNonce, crypto_secretbox_easy, crypto_secretbox_open_easy,
        },
        crypto_sign::{
            crypto_sign_detached, crypto_sign_seed_keypair, crypto_sign_verify_detached,
        },
    },
    constants::{
        CRYPTO_SECRETBOX_KEYBYTES, CRYPTO_SECRETBOX_MACBYTES, CRYPTO_SECRETBOX_NONCEBYTES,
        CRYPTO_SIGN_BYTES, CRYPTO_SIGN_PUBLICKEYBYTES,
    },
    dryocbox::{DryocBox, KeyPair as BoxKeyPair, PublicKey as BoxPublicKey},
    rng::copy_randombytes,
    types::StackByteArray,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Bytes in a challenge nonce.
pub const CHALLENGE_BYTES: usize = 32;
/// Bytes in a data key (XSalsa20-Poly1305 key).
pub const DATA_KEY_BYTES: usize = CRYPTO_SECRETBOX_KEYBYTES;

// ── Identity ──────────────────────────────────────────────────────────────────

/// An Ed25519 signing identity derived from a 32-byte seed.
///
/// The seed is the only secret; the public key is shareable.
/// Deriving from a seed (rather than a random keypair) lets the identity be
/// reproduced deterministically from a stored secret.
pub struct Identity {
    pub_key: [u8; CRYPTO_SIGN_PUBLICKEYBYTES],
    /// dryoc classic SecretKey is 64 bytes (seed || public_key).
    sec_key: [u8; 64],
}

impl Identity {
    /// Derive an identity from a 32-byte secret seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let (pub_key, sec_key) = crypto_sign_seed_keypair(seed);
        Self { pub_key, sec_key }
    }

    /// The Ed25519 public key (32 bytes).
    pub fn public_key(&self) -> [u8; CRYPTO_SIGN_PUBLICKEYBYTES] {
        self.pub_key
    }

    /// Sign `msg` with this identity, returning a 64-byte detached signature.
    pub fn sign(&self, msg: &[u8]) -> Result<[u8; CRYPTO_SIGN_BYTES]> {
        let mut sig = [0u8; CRYPTO_SIGN_BYTES];
        crypto_sign_detached(&mut sig, msg, &self.sec_key)
            .map_err(|e| anyhow!("sign failed: {e}"))?;
        Ok(sig)
    }
}

// ── Challenge-response ────────────────────────────────────────────────────────

/// Issue a random 32-byte challenge nonce.
pub fn issue_challenge() -> [u8; CHALLENGE_BYTES] {
    let mut arr = [0u8; CHALLENGE_BYTES];
    copy_randombytes(&mut arr);
    arr
}

/// Client: sign the challenge with the client identity.
pub fn respond(
    identity: &Identity,
    challenge: &[u8; CHALLENGE_BYTES],
) -> Result<[u8; CRYPTO_SIGN_BYTES]> {
    identity.sign(challenge)
}

/// Server: verify that `sig` is `pubkey`'s signature over `challenge`.
///
/// Returns `Ok(())` on success, `Err` on failure or tampered input.
pub fn verify_response(
    pubkey: &[u8; CRYPTO_SIGN_PUBLICKEYBYTES],
    challenge: &[u8; CHALLENGE_BYTES],
    sig: &[u8; CRYPTO_SIGN_BYTES],
) -> Result<()> {
    crypto_sign_verify_detached(sig, challenge, pubkey)
        .map_err(|e| anyhow!("signature verification failed: {e}"))
}

// ── Per-session data key + secretbox seal/open ────────────────────────────────

/// A random 32-byte symmetric data key.
pub type DataKey = [u8; DATA_KEY_BYTES];

/// Generate a fresh random data key for a new session.
pub fn generate_data_key() -> DataKey {
    let mut key = [0u8; DATA_KEY_BYTES];
    copy_randombytes(&mut key);
    key
}

/// Sealed payload produced by `seal`.
pub struct SealedPayload {
    /// 24-byte XSalsa20-Poly1305 nonce.
    pub nonce: [u8; CRYPTO_SECRETBOX_NONCEBYTES],
    /// Ciphertext with prepended 16-byte MAC
    /// (total length = plaintext.len() + CRYPTO_SECRETBOX_MACBYTES).
    pub ciphertext: Vec<u8>,
}

/// Encrypt `plaintext` with `data_key`.
///
/// Uses XSalsa20-Poly1305 (libsodium `crypto_secretbox_easy`). A fresh random
/// nonce is generated per call.
///
/// ### Data-residency note
/// The caller controls the `data_key`; this function never persists it.
pub fn seal(data_key: &DataKey, plaintext: &[u8]) -> SealedPayload {
    let mut nonce = [0u8; CRYPTO_SECRETBOX_NONCEBYTES];
    copy_randombytes(&mut nonce);

    // SbKey and SbNonce are [u8; N] type aliases in the classic API.
    let key: SbKey = *data_key;
    let nonce_arr: SbNonce = nonce;

    let mut ciphertext = vec![0u8; plaintext.len() + CRYPTO_SECRETBOX_MACBYTES];
    crypto_secretbox_easy(&mut ciphertext, plaintext, &nonce_arr, &key)
        .expect("secretbox_easy must not fail for valid key/nonce lengths");

    SealedPayload { nonce, ciphertext }
}

/// Decrypt a `SealedPayload` with `data_key`.
pub fn open(data_key: &DataKey, payload: &SealedPayload) -> Result<Vec<u8>> {
    if payload.ciphertext.len() < CRYPTO_SECRETBOX_MACBYTES {
        return Err(anyhow!("ciphertext too short"));
    }
    let key: SbKey = *data_key;
    let nonce_arr: SbNonce = payload.nonce;
    let mut plaintext = vec![0u8; payload.ciphertext.len() - CRYPTO_SECRETBOX_MACBYTES];
    crypto_secretbox_open_easy(&mut plaintext, &payload.ciphertext, &nonce_arr, &key)
        .map_err(|e| anyhow!("decryption failed: {e}"))?;
    Ok(plaintext)
}

// ── Key wrapping (X25519 sealed box) ─────────────────────────────────────────

/// A data key wrapped for a specific recipient.
///
/// The relay stores only `WrappedKey` blobs — never the plaintext data key.
/// Only the holder of the corresponding X25519 secret key can unwrap.
pub struct WrappedKey {
    /// Sealed-box bytes: ephemeral pubkey (32) + ciphertext + MAC (16 + 16).
    pub bytes: Vec<u8>,
}

/// A recipient X25519 keypair used for key unwrapping.
///
/// The public key is shareable; the secret key stays with the client.
pub struct RecipientKeypair {
    inner: BoxKeyPair,
}

impl RecipientKeypair {
    /// Generate a fresh random recipient keypair.
    pub fn generate() -> Self {
        Self { inner: BoxKeyPair::generate() }
    }

    /// Public key (32 bytes), share with the server so it can wrap keys for you.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        use dryoc::types::ByteArray as _;
        *self.inner.public_key.as_array()
    }
}

/// Wrap `data_key` for `recipient_pub` (32-byte X25519 public key).
///
/// Uses an ephemeral X25519 keypair + `crypto_box_seal`.  The server calls
/// this to produce a `WrappedKey` it can store; it does NOT hold the data key.
pub fn wrap_key(data_key: &DataKey, recipient_pub: &[u8; 32]) -> Result<WrappedKey> {
    let pub_key: BoxPublicKey = StackByteArray::try_from(recipient_pub.as_slice())
        .map_err(|_| anyhow!("invalid recipient pubkey length"))?;
    let boxed = DryocBox::seal_to_vecbox(data_key, &pub_key)
        .map_err(|e| anyhow!("key wrap failed: {e}"))?;
    Ok(WrappedKey { bytes: boxed.to_vec() })
}

/// Unwrap a `WrappedKey` using the recipient's secret keypair.
///
/// Only the holder of the secret key paired with `recipient.public_key_bytes()`
/// can recover the data key.
pub fn unwrap_key(wrapped: &WrappedKey, recipient: &RecipientKeypair) -> Result<DataKey> {
    // `seal_to_vecbox` produces sealed-box format: [ephemeral_pk(32) | mac(16) | ciphertext].
    // Use `from_sealed_bytes` (not `from_bytes`) to parse this correctly.
    let boxed = DryocBox::from_sealed_bytes(&wrapped.bytes)
        .map_err(|e| anyhow!("invalid sealed box bytes: {e}"))?;
    let plaintext = boxed
        .unseal_to_vec(&recipient.inner)
        .map_err(|e| anyhow!("key unwrap failed: {e}"))?;
    if plaintext.len() != DATA_KEY_BYTES {
        return Err(anyhow!(
            "unwrapped key length mismatch: got {}, expected {}",
            plaintext.len(),
            DATA_KEY_BYTES
        ));
    }
    let mut key = [0u8; DATA_KEY_BYTES];
    key.copy_from_slice(&plaintext);
    Ok(key)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn random_seed() -> [u8; 32] {
        let mut arr = [0u8; 32];
        copy_randombytes(&mut arr);
        arr
    }

    // ── F4-1: sign/verify round-trip ──────────────────────────────────────────

    #[test]
    fn sign_verify_roundtrip() {
        let seed = random_seed();
        let identity = Identity::from_seed(&seed);
        let pubkey = identity.public_key();
        let challenge = issue_challenge();
        let response = respond(&identity, &challenge).unwrap();
        verify_response(&pubkey, &challenge, &response).expect("valid response must verify");
    }

    // ── F4-2a: tampered signature rejected ────────────────────────────────────

    #[test]
    fn sign_tamper_rejection() {
        let seed = random_seed();
        let identity = Identity::from_seed(&seed);
        let pubkey = identity.public_key();
        let challenge = issue_challenge();
        let mut sig = respond(&identity, &challenge).unwrap();

        // Flip a byte in the signature.
        sig[0] ^= 0xFF;
        verify_response(&pubkey, &challenge, &sig)
            .expect_err("tampered signature must be rejected");
    }

    // ── F4-2b: challenge-response with wrong key rejected ─────────────────────

    #[test]
    fn challenge_response_wrong_key_rejected() {
        let identity_a = Identity::from_seed(&random_seed());
        let identity_b = Identity::from_seed(&random_seed());
        let pubkey_a = identity_a.public_key();
        let challenge = issue_challenge();
        // B signs, but we verify against A's public key.
        let resp = respond(&identity_b, &challenge).unwrap();
        verify_response(&pubkey_a, &challenge, &resp)
            .expect_err("response from wrong key must be rejected");
    }

    // ── F4-3a: seal/open round-trip ───────────────────────────────────────────

    #[test]
    fn seal_open_roundtrip() {
        let key = generate_data_key();
        let plaintext = b"session event payload";
        let sealed = seal(&key, plaintext);
        let recovered = open(&key, &sealed).expect("open must succeed with correct key");
        assert_eq!(recovered, plaintext);
    }

    // ── F4-3b: open with wrong key fails ──────────────────────────────────────

    #[test]
    fn open_with_wrong_key_fails() {
        let key = generate_data_key();
        let wrong_key = generate_data_key();
        let plaintext = b"secret data";
        let sealed = seal(&key, plaintext);
        open(&wrong_key, &sealed).expect_err("open with wrong key must fail");
    }

    // ── F4-4a: wrap/unwrap round-trip ─────────────────────────────────────────

    #[test]
    fn wrap_unwrap_roundtrip() {
        let data_key = generate_data_key();
        let recipient = RecipientKeypair::generate();
        let pub_key = recipient.public_key_bytes();
        let wrapped = wrap_key(&data_key, &pub_key).expect("wrap must succeed");
        let recovered =
            unwrap_key(&wrapped, &recipient).expect("unwrap must succeed with correct key");
        assert_eq!(recovered, data_key);
    }

    // ── F4-4b: unwrap with wrong recipient fails ───────────────────────────────

    #[test]
    fn unwrap_wrong_recipient_fails() {
        let data_key = generate_data_key();
        let recipient_a = RecipientKeypair::generate();
        let recipient_b = RecipientKeypair::generate();
        let wrapped =
            wrap_key(&data_key, &recipient_a.public_key_bytes()).expect("wrap must succeed");
        unwrap_key(&wrapped, &recipient_b)
            .expect_err("unwrap with wrong recipient secret must fail");
    }
}
