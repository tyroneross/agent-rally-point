// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Deferred-surface seams (chunk E2): compiling interfaces for the
//! device/account/network-gated transport pieces that the autonomous build
//! cannot verify. These keep the architecture honest — the extension points
//! exist and compile — without claiming the gated functionality works.
//!
//! See `docs/plans/DEFERRED.md` for the human step that finishes each.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Authentication strategy for an incoming client. v1 ships `DevTokenAuth`;
/// `MtlsAuth` is the Secure-Enclave path that replaces it on a real device.
pub trait AuthProvider: Send + Sync {
    /// Return true if the presented credential authenticates the client.
    fn authenticate(&self, presented_token: &str) -> bool;
    /// Human-readable name for logs/audit.
    fn scheme(&self) -> &'static str;
}

/// v1 auth: a shared bearer token (matches `transport::auth::validate_token`).
/// `[CLEANUP]` — replaced by [`MtlsAuth`] as the default once a device exists.
pub struct DevTokenAuth {
    expected: String,
}

impl DevTokenAuth {
    pub fn new(expected: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
        }
    }
}

impl AuthProvider for DevTokenAuth {
    fn authenticate(&self, presented_token: &str) -> bool {
        !self.expected.is_empty() && presented_token == self.expected
    }
    fn scheme(&self) -> &'static str {
        "dev-token"
    }
}

/// TAG:UNTESTED — Secure-Enclave mutual-TLS auth. On a physical iPhone the client
/// presents a cert whose non-exportable key lives in the Secure Enclave, gated by
/// Face ID. Verifying that handshake needs a device + issued cert, so this is a
/// compiling stub that always denies (fail-closed) until wired. See DEFERRED.md.
pub struct MtlsAuth;

impl AuthProvider for MtlsAuth {
    fn authenticate(&self, _presented_token: &str) -> bool {
        // Fail-closed: never silently accept until the real handshake is wired.
        false
    }
    fn scheme(&self) -> &'static str {
        "mtls-secure-enclave (stub)"
    }
}

/// Where the daemon should bind. v1 binds loopback; on a deployed always-on Mac
/// the operator sets this to the Tailscale tailnet interface address.
///
/// TAG:UNTESTED — tailnet binding needs Tailscale installed + a joined tailnet
/// (absent in the autonomous build env). `resolve()` returns loopback by default
/// and echoes an explicit tailnet addr when provided. See DEFERRED.md.
#[derive(Debug, Clone)]
pub enum BindTarget {
    /// Loopback only (v1 / simulator).
    Loopback { port: u16 },
    /// An explicit tailnet interface address supplied by the operator.
    Tailnet { addr: SocketAddr },
}

impl BindTarget {
    pub fn resolve(&self) -> SocketAddr {
        match self {
            BindTarget::Loopback { port } => {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port)
            }
            BindTarget::Tailnet { addr } => *addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_token_auth_matches_and_rejects() {
        let a = DevTokenAuth::new("secret");
        assert!(a.authenticate("secret"));
        assert!(!a.authenticate("nope"));
        assert!(!DevTokenAuth::new("").authenticate("")); // empty never authenticates
        assert_eq!(a.scheme(), "dev-token");
    }

    #[test]
    fn mtls_stub_fails_closed() {
        // The stub must never accept — fail-closed until a device wires it.
        assert!(!MtlsAuth.authenticate("anything"));
        assert!(MtlsAuth.scheme().contains("stub"));
    }

    #[test]
    fn bind_target_resolves() {
        assert_eq!(
            BindTarget::Loopback { port: 8787 }.resolve(),
            "127.0.0.1:8787".parse().unwrap()
        );
        let tn: SocketAddr = "100.64.0.1:8787".parse().unwrap();
        assert_eq!(BindTarget::Tailnet { addr: tn }.resolve(), tn);
    }
}
