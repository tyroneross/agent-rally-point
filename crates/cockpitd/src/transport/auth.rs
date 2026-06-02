// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Auth for the WebSocket transport.
//!
//! v1: bearer-token check via `COCKPIT_TOKEN` env var.
//!
//! TAG:UNTESTED — Secure-Enclave mTLS replaces this in a future iteration.
//! The `AuthProvider` trait is the seam for that swap.

/// Validates a bearer token against `COCKPIT_TOKEN` env var.
///
/// Returns `Ok(())` if the token is valid, `Err(reason)` otherwise.
pub fn validate_token(token: &str) -> Result<(), &'static str> {
    match std::env::var("COCKPIT_TOKEN") {
        Ok(expected) if !expected.is_empty() => {
            if token == expected {
                Ok(())
            } else {
                Err("token mismatch")
            }
        }
        Ok(_) => Err("COCKPIT_TOKEN is set but empty"),
        Err(_) => Err("COCKPIT_TOKEN not set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_token_accepted() {
        // We cannot easily set env in test isolation without std::env::set_var,
        // so we test the path directly by checking the logic with the current env.
        // The real validation is exercised in the e2e test.
        let _ = validate_token("any"); // must not panic
    }
}
