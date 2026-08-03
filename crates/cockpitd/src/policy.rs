// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Deployment policy read from the environment (ARP-005).
//!
//! Two guards live here. The bind guard is fail-closed. The repo allowlist is
//! fail-closed only once configured — **its default is `$HOME`**, which is a
//! blast-radius cut, not a sandbox: it keeps a stolen token out of `/etc`,
//! `/usr`, and other users' homes while a personal daemon keeps working. A
//! token holder can still launch an agent at `~/Downloads`, `~/.ssh`, or
//! `~/.aws` unless `COCKPIT_REPO_ALLOWLIST` names narrower roots. Set it.
//! (Set-but-empty means allow nothing, which IS fail-closed.)
//!
//! 1. **Repo allowlist.** `launch_session { repo_path }` becomes the child
//!    agent's working directory. Without a bound, a token holder can start an
//!    agent in any path the daemon can read. `resolve_repo_path` canonicalizes
//!    the request *first* and then requires it to sit inside a configured root,
//!    so `..` traversal and symlinks that point outside a root are rejected.
//!
//! 2. **Bind address.** Loopback is the default and needs no ceremony. Any
//!    other address — including `0.0.0.0` — is refused unless the operator sets
//!    `COCKPIT_ALLOW_NON_LOOPBACK=i-understand-the-risk`. The daemon has one
//!    shared bearer token and no transport identity; exposing it beyond
//!    loopback hands whoever reaches the port full control of every session.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::warn;

/// Colon-separated list of directories a session may be launched in.
pub const REPO_ALLOWLIST_ENV: &str = "COCKPIT_REPO_ALLOWLIST";

/// Opt-in acknowledgement required before binding a non-loopback address.
pub const NON_LOOPBACK_ENV: &str = "COCKPIT_ALLOW_NON_LOOPBACK";

/// The exact value `COCKPIT_ALLOW_NON_LOOPBACK` must carry. Spelled out so it
/// cannot be set by accident or by a generic "enable everything" script.
pub const NON_LOOPBACK_ACK: &str = "i-understand-the-risk";

// ── Repo allowlist ────────────────────────────────────────────────────────────

/// Why a requested `repo_path` was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoPathRejection {
    #[error(
        "no repo roots are configured, so every repo_path is refused. \
         Set {REPO_ALLOWLIST_ENV}=<colon-separated absolute directories> \
         (for example {REPO_ALLOWLIST_ENV}=$HOME/dev) and restart cockpitd."
    )]
    NoRootsConfigured,

    #[error("repo_path '{requested}' cannot be resolved: {reason}")]
    Unresolvable { requested: String, reason: String },

    #[error("repo_path '{requested}' resolves to '{resolved}', which is not a directory")]
    NotADirectory { requested: String, resolved: String },

    #[error(
        "repo_path '{requested}' resolves to '{resolved}', which is outside the allowed roots [{}]. \
         Add the root to {REPO_ALLOWLIST_ENV} (colon-separated) and restart cockpitd if this path is intended.",
        .roots.join(", ")
    )]
    OutsideAllowlist {
        requested: String,
        resolved: String,
        roots: Vec<String>,
    },
}

/// Parse the allowlist spec into roots.
///
/// - `spec = None` (variable unset) → the single default root, `home`.
/// - `spec = Some("")` or only separators → no roots. Every launch is refused.
/// - otherwise → each non-empty colon-separated entry, in order.
///
/// The default is `$HOME` rather than "anything". That still covers the whole
/// operator account, so it is a blast-radius cut and not a sandbox: it keeps a
/// stolen token out of `/etc`, `/usr`, `/var`, and other users' homes. Set
/// `COCKPIT_REPO_ALLOWLIST` to the directories you actually check out into.
pub fn parse_roots(spec: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    match spec {
        Some(s) => s
            .split(':')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(PathBuf::from)
            .collect(),
        None => home
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| vec![PathBuf::from(h)])
            .unwrap_or_default(),
    }
}

/// The roots configured for this process.
pub fn configured_roots() -> Vec<PathBuf> {
    let spec = std::env::var(REPO_ALLOWLIST_ENV).ok();
    let home = std::env::var("HOME").ok();
    parse_roots(spec.as_deref(), home.as_deref())
}

/// Canonicalize `requested` and require it to sit inside one of `roots`.
///
/// Canonicalization happens **before** the containment check, which is what
/// makes `<root>/../../etc` and a symlink pointing out of a root both fail.
/// Roots are canonicalized too, so a root written as `/tmp` still matches a
/// request under macOS's real `/private/tmp`.
///
/// Returns the canonical path to hand to the adapter. Passing the canonical
/// form onward means the child process gets exactly the directory that was
/// checked, with no second resolution in between.
pub fn resolve_repo_path_within(
    requested: &str,
    roots: &[PathBuf],
) -> Result<PathBuf, RepoPathRejection> {
    if roots.is_empty() {
        return Err(RepoPathRejection::NoRootsConfigured);
    }

    let resolved =
        std::fs::canonicalize(requested).map_err(|e| RepoPathRejection::Unresolvable {
            requested: requested.to_string(),
            reason: e.to_string(),
        })?;

    if !resolved.is_dir() {
        return Err(RepoPathRejection::NotADirectory {
            requested: requested.to_string(),
            resolved: resolved.display().to_string(),
        });
    }

    // A root that does not exist cannot contain anything; drop it rather than
    // failing the whole check, but say so once at warn level.
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| match std::fs::canonicalize(r) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("repo allowlist root {} is unusable: {e}", r.display());
                None
            }
        })
        .collect();

    // Path::starts_with compares whole components, so root `/srv/repo` does not
    // match `/srv/repo-evil`.
    if canonical_roots.iter().any(|r| resolved.starts_with(r)) {
        return Ok(resolved);
    }

    Err(RepoPathRejection::OutsideAllowlist {
        requested: requested.to_string(),
        resolved: resolved.display().to_string(),
        roots: roots.iter().map(|r| r.display().to_string()).collect(),
    })
}

/// Env-backed wrapper over [`resolve_repo_path_within`].
pub fn resolve_repo_path(requested: &str) -> Result<PathBuf, RepoPathRejection> {
    resolve_repo_path_within(requested, &configured_roots())
}

// ── Bind address ──────────────────────────────────────────────────────────────

/// Why a bind address was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "refusing to bind {addr}: it is not a loopback address. cockpitd authenticates with one \
     shared bearer token and has no transport identity, so anything that can reach this port \
     controls every session. Keep COCKPIT_ADDR on 127.0.0.1, or set \
     {NON_LOOPBACK_ENV}={NON_LOOPBACK_ACK} if you have put your own authenticated transport \
     (tailnet ACL, mTLS terminator, SSH tunnel) in front of it."
)]
pub struct NonLoopbackRefused {
    pub addr: SocketAddr,
}

/// Allow the bind only when the address is loopback, or the operator has
/// acknowledged the risk with the exact override value.
///
/// Loopback never requires the override.
pub fn check_bind_addr(
    addr: &SocketAddr,
    override_value: Option<&str>,
) -> Result<(), NonLoopbackRefused> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if override_value.map(str::trim) == Some(NON_LOOPBACK_ACK) {
        warn!(
            "binding non-loopback address {addr} because {NON_LOOPBACK_ENV} is set — \
             every client that reaches this port shares one bearer token"
        );
        return Ok(());
    }
    Err(NonLoopbackRefused { addr: *addr })
}

/// Env-backed wrapper over [`check_bind_addr`].
pub fn check_bind_addr_from_env(addr: &SocketAddr) -> Result<(), NonLoopbackRefused> {
    let ov = std::env::var(NON_LOOPBACK_ENV).ok();
    check_bind_addr(addr, ov.as_deref())
}

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Build the `../`-laden path that climbs out of `root` and lands on `target`.
///
/// Used by the traversal tests: the resulting string only fails the containment
/// check if the implementation canonicalizes before comparing.
pub fn traversal_out_of(root: &Path, target: &str) -> String {
    let depth = root
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    let mut p = root.display().to_string();
    for _ in 0..depth {
        p.push_str("/..");
    }
    p.push('/');
    p.push_str(target.trim_start_matches('/'));
    p
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn unique_dir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("cockpitd-policy-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    /// `/etc` is itself a symlink on macOS (`/private/etc`), so the expected
    /// resolved form is whatever the platform canonicalizes it to.
    fn canonical_etc() -> String {
        std::fs::canonicalize("/etc").unwrap().display().to_string()
    }

    // ── Roots parsing ─────────────────────────────────────────────────────────

    #[test]
    fn unset_allowlist_defaults_to_home() {
        let roots = parse_roots(None, Some("/Users/example"));
        assert_eq!(roots, vec![PathBuf::from("/Users/example")]);
    }

    #[test]
    fn unset_allowlist_without_home_yields_no_roots() {
        assert!(parse_roots(None, None).is_empty());
        assert!(parse_roots(None, Some("")).is_empty());
    }

    #[test]
    fn empty_allowlist_yields_no_roots() {
        // Set-but-empty is an explicit "allow nothing", mirroring COCKPIT_TOKEN.
        assert!(parse_roots(Some(""), Some("/Users/example")).is_empty());
        assert!(parse_roots(Some(":::"), Some("/Users/example")).is_empty());
    }

    #[test]
    fn allowlist_splits_on_colon() {
        let roots = parse_roots(Some("/a:/b: /c "), None);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }

    // ── ARP-005 #4: repo_path containment ─────────────────────────────────────

    #[test]
    fn no_roots_refuses_everything() {
        let err = resolve_repo_path_within("/tmp", &[]).unwrap_err();
        assert_eq!(err, RepoPathRejection::NoRootsConfigured);
        assert!(
            err.to_string().contains(REPO_ALLOWLIST_ENV),
            "the refusal must tell the operator which variable to set: {err}"
        );
    }

    #[test]
    fn path_inside_root_is_allowed() {
        let root = unique_dir("inside");
        let nested = root.join("repo/sub");
        std::fs::create_dir_all(&nested).unwrap();

        let ok = resolve_repo_path_within(nested.to_str().unwrap(), std::slice::from_ref(&root))
            .unwrap();
        assert_eq!(ok, std::fs::canonicalize(&nested).unwrap());

        // The root itself is inside the root.
        assert!(
            resolve_repo_path_within(root.to_str().unwrap(), std::slice::from_ref(&root)).is_ok()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn absolute_system_path_is_rejected() {
        let root = unique_dir("etc-deny");
        let err = resolve_repo_path_within("/etc", std::slice::from_ref(&root)).unwrap_err();
        assert!(
            matches!(err, RepoPathRejection::OutsideAllowlist { .. }),
            "/etc must be refused, got {err:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dotdot_traversal_out_of_root_is_rejected() {
        // This case only fails if the implementation canonicalizes before
        // comparing: the raw string starts with the allowed root.
        let root = unique_dir("traversal");
        let hostile = traversal_out_of(&root, "etc");
        assert!(
            hostile.starts_with(root.to_str().unwrap()),
            "the hostile string must textually start with the root, else the \
             test does not prove canonicalization: {hostile}"
        );

        let err = resolve_repo_path_within(&hostile, std::slice::from_ref(&root)).unwrap_err();
        match err {
            RepoPathRejection::OutsideAllowlist { resolved, .. } => {
                assert_eq!(resolved, canonical_etc(), "traversal must resolve to /etc");
            }
            other => panic!("`..` traversal must be refused as OutsideAllowlist, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_root_is_rejected() {
        let root = unique_dir("symlink");
        let link = root.join("escape");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let err = resolve_repo_path_within(link.to_str().unwrap(), std::slice::from_ref(&root))
            .unwrap_err();
        match err {
            RepoPathRejection::OutsideAllowlist { resolved, .. } => {
                assert_eq!(
                    resolved,
                    canonical_etc(),
                    "symlink must resolve to its target"
                );
            }
            other => panic!("symlink out of the root must be refused, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sibling_with_shared_prefix_is_rejected() {
        // `/x/allowed-evil` must not pass a root of `/x/allowed`.
        let base = unique_dir("prefix");
        let root = base.join("allowed");
        let evil = base.join("allowed-evil");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&evil).unwrap();

        let err = resolve_repo_path_within(evil.to_str().unwrap(), std::slice::from_ref(&root))
            .unwrap_err();
        assert!(
            matches!(err, RepoPathRejection::OutsideAllowlist { .. }),
            "prefix-sharing sibling must be refused, got {err:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn missing_path_is_unresolvable() {
        let root = unique_dir("missing");
        let err = resolve_repo_path_within(
            &format!("{}/nope", root.display()),
            std::slice::from_ref(&root),
        )
        .unwrap_err();
        assert!(
            matches!(err, RepoPathRejection::Unresolvable { .. }),
            "a nonexistent path must be refused, got {err:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_is_not_a_valid_repo_path() {
        let root = unique_dir("file");
        let file = root.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let err = resolve_repo_path_within(file.to_str().unwrap(), std::slice::from_ref(&root))
            .unwrap_err();
        assert!(
            matches!(err, RepoPathRejection::NotADirectory { .. }),
            "a file must be refused, got {err:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nonexistent_root_is_skipped_not_fatal() {
        let root = unique_dir("skip");
        let roots = vec![PathBuf::from("/definitely/not/here"), root.clone()];
        assert!(resolve_repo_path_within(root.to_str().unwrap(), &roots).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    // ── ARP-005 #5: bind refusal ──────────────────────────────────────────────

    #[test]
    fn loopback_binds_without_override() {
        let v4: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8787);
        assert!(check_bind_addr(&v4, None).is_ok());
        assert!(check_bind_addr(&v6, None).is_ok());
        // 127.0.0.0/8 is all loopback.
        assert!(check_bind_addr(&"127.9.9.9:1".parse().unwrap(), None).is_ok());
    }

    #[test]
    fn non_loopback_is_refused_without_override() {
        for a in ["0.0.0.0:8787", "192.168.1.10:8787", "[::]:8787"] {
            let addr: SocketAddr = a.parse().unwrap();
            let err = check_bind_addr(&addr, None)
                .unwrap_err_or_panic(&format!("{a} must be refused without the override"));
            assert_eq!(err.addr, addr);
            assert!(
                err.to_string().contains(NON_LOOPBACK_ENV),
                "the refusal must name the override variable: {err}"
            );
        }
    }

    #[test]
    fn wrong_override_value_still_refuses() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8787);
        assert!(check_bind_addr(&addr, Some("1")).is_err());
        assert!(check_bind_addr(&addr, Some("true")).is_err());
        assert!(check_bind_addr(&addr, Some("yes")).is_err());
        assert!(check_bind_addr(&addr, Some("")).is_err());
    }

    #[test]
    fn exact_override_value_permits_non_loopback() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8787);
        assert!(check_bind_addr(&addr, Some(NON_LOOPBACK_ACK)).is_ok());
        assert!(check_bind_addr(&addr, Some(" i-understand-the-risk ")).is_ok());
    }

    /// Tiny helper so the loop above reads as one assertion per address.
    trait UnwrapErrOrPanic<T, E> {
        fn unwrap_err_or_panic(self, msg: &str) -> E;
    }
    impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<T, E> for Result<T, E> {
        fn unwrap_err_or_panic(self, msg: &str) -> E {
            match self {
                Ok(v) => panic!("{msg} — got Ok({v:?})"),
                Err(e) => e,
            }
        }
    }
}
