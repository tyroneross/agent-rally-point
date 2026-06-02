// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Zero-knowledge relay transport seam (§8 F5).
//!
//! `ZeroKnowledgeRelay` implements the `Transport` trait and routes **opaque
//! encrypted envelopes** between clients by room (user/session).  It MUST
//! NEVER be able to read plaintext — it sees only:
//! - A `room_id` (routing metadata)
//! - The ciphertext blob (sealed by the sender with a key the relay never holds)
//!
//! ### Data-residency invariant (§3)
//! The relay stores nothing in plaintext.  It holds zero data keys.  It
//! forwards sealed bytes it cannot decrypt.  This satisfies §3's rule:
//! > No server may store this data in plaintext, ever.
//!
//! ### Architecture
//! Clients are modelled as `RelayClient` objects.  Each client registers
//! with a `room_id` and supplies its X25519 public key (for key distribution,
//! not used by the relay itself — the relay never uses it to encrypt/decrypt).
//! The relay routes sealed envelopes to other clients in the same room.
//!
//! ### In-process use (F5 seam)
//! This module is an **in-process** relay for testing and for the loopback /
//! tailnet case where two local clients need a mediated channel.  A future
//! network relay would implement the same envelope contract over WebSocket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::mpsc;

// ── Wire envelope ─────────────────────────────────────────────────────────────

/// An opaque sealed envelope routed by the relay.
///
/// The relay only inspects `room_id` for routing; it cannot read `payload`.
#[derive(Clone, Debug)]
pub struct SealedEnvelope {
    /// Room (user/session) this message targets.
    pub room_id: String,
    /// Sender's client id (for loop-back avoidance / audit).
    pub sender_id: String,
    /// Opaque ciphertext.  The relay forwards this verbatim and NEVER decrypts it.
    pub payload: Vec<u8>,
}

// ── Relay state ───────────────────────────────────────────────────────────────

/// Per-client entry tracked by the relay.
struct ClientEntry {
    /// Sender half for pushing envelopes to this client.
    tx: mpsc::Sender<SealedEnvelope>,
    /// X25519 public key (32 bytes) — held for key-distribution; relay never
    /// uses this to encrypt/decrypt.
    pubkey: [u8; 32],
}

/// Shared inner state of the relay (protected by a Mutex).
struct RelayInner {
    /// room_id → list of registered clients.
    rooms: HashMap<String, Vec<(String, ClientEntry)>>,
}

impl RelayInner {
    fn new() -> Self {
        Self { rooms: HashMap::new() }
    }

    fn register(
        &mut self,
        room_id: &str,
        client_id: &str,
        pubkey: [u8; 32],
        tx: mpsc::Sender<SealedEnvelope>,
    ) {
        self.rooms
            .entry(room_id.to_string())
            .or_default()
            .push((
                client_id.to_string(),
                ClientEntry { tx, pubkey },
            ));
    }

    fn deregister(&mut self, room_id: &str, client_id: &str) {
        if let Some(members) = self.rooms.get_mut(room_id) {
            members.retain(|(id, _)| id != client_id);
        }
    }

    /// Get the public key of a client in a room.
    fn get_pubkey(&self, room_id: &str, client_id: &str) -> Option<[u8; 32]> {
        self.rooms.get(room_id)?.iter().find_map(|(id, entry)| {
            if id == client_id { Some(entry.pubkey) } else { None }
        })
    }

}

// ── ZeroKnowledgeRelay ────────────────────────────────────────────────────────

/// In-process zero-knowledge relay.
///
/// Routes encrypted envelopes between `RelayClient` objects without ever
/// decrypting payloads.  Thread-safe via `Arc<Mutex<RelayInner>>`.
#[derive(Clone)]
pub struct ZeroKnowledgeRelay {
    inner: Arc<Mutex<RelayInner>>,
}

impl ZeroKnowledgeRelay {
    /// Create a new, empty relay.
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(RelayInner::new())) }
    }

    /// Join a room, returning a `RelayClient` handle.
    ///
    /// `pubkey` is the client's X25519 public key (for key-distribution by
    /// callers; the relay itself never uses it to encrypt).
    pub fn join(
        &self,
        room_id: impl Into<String>,
        client_id: impl Into<String>,
        pubkey: [u8; 32],
    ) -> RelayClient {
        let room_id = room_id.into();
        let client_id = client_id.into();
        let (tx, rx) = mpsc::channel::<SealedEnvelope>(64);
        {
            let mut inner = self.inner.lock().expect("relay mutex poisoned");
            inner.register(&room_id, &client_id, pubkey, tx);
        }
        RelayClient {
            relay: self.clone(),
            room_id,
            client_id,
            rx,
        }
    }

    /// Look up the public key of a peer in `room_id`.
    ///
    /// Used by clients to find the recipient pubkey for key wrapping; the relay
    /// itself never calls this internally.
    pub fn get_peer_pubkey(&self, room_id: &str, client_id: &str) -> Option<[u8; 32]> {
        self.inner.lock().expect("relay mutex poisoned")
            .get_pubkey(room_id, client_id)
    }

    async fn forward(&self, envelope: SealedEnvelope) {
        // Collect senders while holding the lock, then release before awaiting.
        // Holding a Mutex guard across an `.await` point is not permitted in Tokio.
        let senders: Vec<mpsc::Sender<SealedEnvelope>> = {
            let inner = self.inner.lock().expect("relay mutex poisoned");
            inner
                .rooms
                .get(&envelope.room_id)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(|(id, entry)| {
                            if id != &envelope.sender_id {
                                Some(entry.tx.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
            // MutexGuard dropped here.
        };
        for tx in senders {
            let _ = tx.send(envelope.clone()).await;
        }
    }
}

impl Default for ZeroKnowledgeRelay {
    fn default() -> Self {
        Self::new()
    }
}

// ── RelayClient ───────────────────────────────────────────────────────────────

/// A client handle connected to a `ZeroKnowledgeRelay` room.
pub struct RelayClient {
    relay: ZeroKnowledgeRelay,
    pub room_id: String,
    pub client_id: String,
    rx: mpsc::Receiver<SealedEnvelope>,
}

impl RelayClient {
    /// Send a sealed payload to all other clients in the room.
    ///
    /// The relay forwards `ciphertext` verbatim.  It cannot decrypt it.
    pub async fn send(&self, ciphertext: Vec<u8>) -> Result<()> {
        let envelope = SealedEnvelope {
            room_id: self.room_id.clone(),
            sender_id: self.client_id.clone(),
            payload: ciphertext,
        };
        self.relay.forward(envelope).await;
        Ok(())
    }

    /// Wait for the next sealed envelope addressed to this client.
    pub async fn recv(&mut self) -> Option<SealedEnvelope> {
        self.rx.recv().await
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.relay.inner.lock() {
            inner.deregister(&self.room_id, &self.client_id);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{
        RecipientKeypair, generate_data_key, open, seal, unwrap_key, wrap_key,
    };

    /// F5 end-to-end: two in-process clients exchange a message through the relay.
    ///
    /// ### Zero-knowledge assertion
    /// After the test we assert that:
    /// 1. The `intercepted` bytes (what the relay forwarded) are NOT equal to
    ///    the plaintext — the relay sees only ciphertext.
    /// 2. The recipient decrypts correctly — proving the data key works.
    /// 3. The relay never had access to the data key — we keep it out of the relay
    ///    and show that without it the raw payload cannot be interpreted as plaintext.
    #[tokio::test]
    async fn two_clients_end_to_end_through_relay_zero_knowledge() {
        let relay = ZeroKnowledgeRelay::new();

        // ── Recipient setup ──────────────────────────────────────────────────
        // Recipient generates an X25519 keypair; shares only the public key.
        let recipient_kp = RecipientKeypair::generate();
        let recipient_pubkey = recipient_kp.public_key_bytes();

        // ── Join room ────────────────────────────────────────────────────────
        let sender_client = relay.join("room:session-42", "sender", [0u8; 32]);
        let mut recipient_client =
            relay.join("room:session-42", "recipient", recipient_pubkey);

        // ── Session data key (never given to relay) ──────────────────────────
        let data_key = generate_data_key();

        // ── Sender: wrap key for recipient and seal the payload ───────────────
        // In a real system the sender would fetch recipient's pubkey via some
        // key-distribution mechanism.  Here we use relay.get_peer_pubkey().
        let peer_pub = relay
            .get_peer_pubkey("room:session-42", "recipient")
            .expect("recipient registered their pubkey");

        let wrapped_key = wrap_key(&data_key, &peer_pub).expect("wrap must succeed");
        let plaintext = b"agent event: tool_call { cmd: 'ls -la' }";
        let sealed = seal(&data_key, plaintext);

        // ── Build the envelope payload: [wrapped_key_len(4) || wrapped_key || nonce(24) || ciphertext]
        // (A real protocol would have a proper framing; this is sufficient for the test.)
        let wk_bytes = &wrapped_key.bytes;
        let wk_len = (wk_bytes.len() as u32).to_le_bytes();
        let mut envelope_payload: Vec<u8> = Vec::new();
        envelope_payload.extend_from_slice(&wk_len);
        envelope_payload.extend_from_slice(wk_bytes);
        envelope_payload.extend_from_slice(&sealed.nonce);
        envelope_payload.extend_from_slice(&sealed.ciphertext);

        // ── Sender transmits — relay sees only ciphertext ─────────────────────
        sender_client
            .send(envelope_payload.clone())
            .await
            .expect("send must succeed");

        // ── Relay has forwarded; intercept from recipient's channel ───────────
        let received_envelope = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            recipient_client.recv(),
        )
        .await
        .expect("recv must not time out")
        .expect("envelope must arrive");

        let intercepted = &received_envelope.payload;

        // ── Zero-knowledge assertion 1: intercepted != plaintext ──────────────
        assert_ne!(
            intercepted.as_slice(),
            plaintext.as_slice(),
            "the relay must never forward plaintext"
        );

        // ── Zero-knowledge assertion 2: relay cannot decrypt payload ──────────
        // The relay had no data_key.  We simulate what the relay could see by
        // treating intercepted as plaintext — it should not match.
        let relay_attempt_str = String::from_utf8_lossy(intercepted);
        assert!(
            !relay_attempt_str.contains("tool_call"),
            "relay payload must not contain readable plaintext"
        );

        // ── Recipient: unwrap data key and decrypt ────────────────────────────
        let received = intercepted.as_slice();
        assert!(received.len() >= 4, "envelope must have length prefix");
        let wk_len_recv = u32::from_le_bytes(received[..4].try_into().unwrap()) as usize;
        let wk_bytes_recv = &received[4..4 + wk_len_recv];
        let nonce_start = 4 + wk_len_recv;
        let nonce_recv: [u8; 24] = received[nonce_start..nonce_start + 24]
            .try_into()
            .expect("nonce must be 24 bytes");
        let ciphertext_recv = received[nonce_start + 24..].to_vec();

        let wrapped_recv = crate::crypto::WrappedKey { bytes: wk_bytes_recv.to_vec() };
        let recovered_key = unwrap_key(&wrapped_recv, &recipient_kp)
            .expect("recipient must be able to unwrap key");

        let sealed_recv = crate::crypto::SealedPayload {
            nonce: nonce_recv,
            ciphertext: ciphertext_recv,
        };
        let decrypted = open(&recovered_key, &sealed_recv)
            .expect("recipient must decrypt successfully");

        assert_eq!(decrypted, plaintext, "recipient must recover exact plaintext");
    }

    /// Verify that a client in one room does NOT receive messages from another room.
    #[tokio::test]
    async fn messages_do_not_cross_rooms() {
        let relay = ZeroKnowledgeRelay::new();
        let client_a = relay.join("room:alpha", "client-a", [1u8; 32]);
        let mut client_b = relay.join("room:beta", "client-b", [2u8; 32]);
        let mut client_c = relay.join("room:alpha", "client-c", [3u8; 32]);

        // Send from A (room alpha) — only C should receive it.
        client_a.send(b"hello alpha".to_vec()).await.unwrap();

        // C receives the message.
        let env = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            client_c.recv(),
        )
        .await
        .expect("C must receive")
        .expect("envelope present");
        assert_eq!(env.payload, b"hello alpha");

        // B (room beta) must NOT receive anything.
        let b_result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            client_b.recv(),
        )
        .await;
        assert!(b_result.is_err(), "B must not receive messages from room alpha");
    }
}
