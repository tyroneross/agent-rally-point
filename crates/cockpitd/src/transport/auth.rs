// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Auth for the WebSocket transport.
//!
//! v1: bearer-token check via `COCKPIT_TOKEN` env var, plus a per-connection
//! `Principal` that becomes the `owner_id` of every session the connection
//! launches.
//!
//! ## What the token proves, and what it does not (ARP-005)
//!
//! One token guards the whole daemon. A caller who holds it is authenticated,
//! full stop — there is no per-client credential. `Principal` therefore is not
//! an authentication boundary. It is an *ownership* label:
//!
//! - Each connection gets a fresh principal, so two concurrent clients using
//!   the same token cannot send, steer, close, or approve each other's
//!   sessions by accident or by scanning UUIDs.
//! - A client that wants to keep control of its sessions across reconnects may
//!   send `client_id` in the `hello` frame. That value is **self-asserted**.
//!   Any holder of the token can claim any `client_id` and inherit those
//!   sessions. It stops accidents, not attackers.
//!
//! Real isolation needs per-client credentials (Secure-Enclave mTLS is the
//! planned replacement). The `AuthProvider` trait in `seams.rs` is that seam.
//! Until it lands, treat the token as a single shared root credential.

use uuid::Uuid;

/// Constant-time byte-slice equality.
///
/// The loop runs `max(a.len(), b.len())` times and folds every difference into
/// an accumulator with `|=`. There is no early return, so the time taken does
/// not depend on *where* the first differing byte sits — only on the lengths.
/// Length inequality is folded into the same accumulator rather than checked
/// up front.
///
/// The remaining leak is the length itself, which a bearer token does not try
/// to hide.
///
/// This property is structural (no branch on secret data), not something a test
/// can assert reliably — see the note on `token_length_does_not_short_circuit`.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff: usize = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= (x ^ y) as usize;
    }
    // black_box keeps the optimizer from re-introducing an early exit.
    std::hint::black_box(diff) == 0
}

/// Validates a bearer token against `COCKPIT_TOKEN` env var.
///
/// Fails closed: an unset or empty `COCKPIT_TOKEN` rejects every caller.
///
/// Returns `Ok(())` if the token is valid, `Err(reason)` otherwise.
pub fn validate_token(token: &str) -> Result<(), &'static str> {
    match std::env::var("COCKPIT_TOKEN") {
        Ok(expected) if !expected.is_empty() => {
            if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                Ok(())
            } else {
                Err("token mismatch")
            }
        }
        Ok(_) => Err("COCKPIT_TOKEN is set but empty"),
        Err(_) => Err("COCKPIT_TOKEN not set"),
    }
}

// ── Principal ─────────────────────────────────────────────────────────────────

/// Longest `client_id` a caller may assert. Keeps the stored `owner_id` bounded.
const MAX_CLIENT_ID_LEN: usize = 64;

/// The identity a connection acts as. Stored as `sessions.owner_id`.
///
/// Two shapes:
/// - `conn:<uuid>` — minted per connection when the client asserts nothing.
///   Sessions launched under it die (for control purposes) with the connection.
/// - `client:<id>` — asserted by the client in `hello`. Survives reconnects.
///   Self-asserted, so it is not an authentication boundary (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(String);

impl Principal {
    /// Mint a fresh identity for one connection.
    pub fn per_connection() -> Self {
        Self(format!("conn:{}", Uuid::new_v4()))
    }

    /// Build a stable identity from a client-asserted `client_id`.
    ///
    /// Returns `None` when the value is empty, over `MAX_CLIENT_ID_LEN`, or
    /// contains anything outside `[A-Za-z0-9._-]`. Callers fall back to
    /// `per_connection()` so a malformed assertion degrades to the stricter
    /// behaviour rather than the looser one.
    pub fn from_client_id(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_CLIENT_ID_LEN {
            return None;
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return None;
        }
        Some(Self(format!("client:{trimmed}")))
    }

    /// Resolve the principal for a connection from an optional asserted id.
    pub fn resolve(client_id: Option<&str>) -> Self {
        client_id
            .and_then(Self::from_client_id)
            .unwrap_or_else(Self::per_connection)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ARP-005 #1: constant-time compare is *correct* ────────────────────────
    //
    // We assert behaviour only. A timing assertion would be flaky on a shared
    // CI box; the constant-time property is structural (no early return, no
    // branch on secret bytes) and is reviewed in `constant_time_eq` itself.

    #[test]
    fn constant_time_eq_accepts_identical() {
        assert!(constant_time_eq(b"correct-horse", b"correct-horse"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_same_length_mismatch() {
        // Differs in the last byte — must still reject.
        assert!(!constant_time_eq(b"correct-horse", b"correct-horsE"));
        // Differs in the first byte — must reject identically.
        assert!(!constant_time_eq(b"correct-horse", b"Correct-horse"));
    }

    #[test]
    fn constant_time_eq_rejects_different_length() {
        assert!(!constant_time_eq(b"correct-horse", b"correct-hors"));
        assert!(!constant_time_eq(
            b"correct-horse",
            b"correct-horse-battery"
        ));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(!constant_time_eq(b"x", b""));
    }

    #[test]
    fn constant_time_eq_rejects_prefix_padded_with_nul() {
        // The max-length loop reads 0 past the end of the short slice. A token
        // of NUL bytes must not therefore compare equal to the empty string.
        assert!(!constant_time_eq(b"\0\0\0", b""));
        assert!(!constant_time_eq(b"abc\0", b"abc"));
    }

    #[test]
    fn token_length_does_not_short_circuit() {
        // Structural note, asserted as behaviour: a wrong token that shares a
        // long prefix with the right one is rejected exactly like one that
        // differs immediately.
        let expected = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let near_miss = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
        let far_miss = b"baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(!constant_time_eq(near_miss, expected));
        assert!(!constant_time_eq(far_miss, expected));
    }

    // ── Principal ─────────────────────────────────────────────────────────────

    #[test]
    fn per_connection_principals_are_distinct() {
        let a = Principal::per_connection();
        let b = Principal::per_connection();
        assert_ne!(a, b, "each connection must get its own identity");
        assert!(a.as_str().starts_with("conn:"));
    }

    #[test]
    fn client_id_gives_stable_principal() {
        let a = Principal::resolve(Some("iphone-15"));
        let b = Principal::resolve(Some("iphone-15"));
        assert_eq!(a, b, "same client_id must reconnect to the same identity");
        assert_eq!(a.as_str(), "client:iphone-15");
    }

    #[test]
    fn malformed_client_id_falls_back_to_per_connection() {
        for bad in ["", "   ", "has space", "semi;colon", &"x".repeat(65)] {
            let p = Principal::resolve(Some(bad));
            assert!(
                p.as_str().starts_with("conn:"),
                "malformed client_id {bad:?} must degrade to a per-connection identity, got {p}"
            );
        }
    }

    #[test]
    fn absent_client_id_falls_back_to_per_connection() {
        assert!(Principal::resolve(None).as_str().starts_with("conn:"));
    }
}
