// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Resource id normalization for `--path` arguments on claim/blocker commands.
//!
//! Different invocations referring to the same file must produce the same
//! resource string, otherwise conflict detection silently fails. The Python
//! predecessor (`agent_rally_point.resources.normalize_file_resource`) did
//! `~` expansion, repo-relative POSIX normalization, and `./`-collapse; this
//! module restores that behavior in Rust.

use std::path::{Component, Path, PathBuf};

/// Return a canonical `file:` resource id for `path` interpreted relative to
/// `workdir`. Three cases:
/// - Absolute paths under `workdir` become repo-relative POSIX strings.
/// - Absolute paths outside `workdir` stay absolute POSIX.
/// - Relative paths are normalized (`./`, redundant `.` segments collapsed).
pub(crate) fn normalize_file_resource(path: &str, workdir: &Path) -> String {
    let expanded = expand_tilde(path);
    let p = PathBuf::from(&expanded);
    let normalized = if p.is_absolute() {
        let resolved = canonicalize_or_keep(&p);
        match resolved.strip_prefix(workdir) {
            Ok(rel) => posix(rel),
            Err(_) => posix(&resolved),
        }
    } else {
        // Collapse `.` segments and convert separators to POSIX without
        // resolving symlinks (the path may not exist yet).
        let collapsed: PathBuf = p
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        posix(&collapsed)
    };
    let trimmed = if normalized == "." {
        String::new()
    } else {
        normalized
    };
    format!("file:{trimmed}")
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn canonicalize_or_keep(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workdir() -> PathBuf {
        PathBuf::from("/tmp/repo")
    }

    #[test]
    fn relative_path_drops_dot_prefix() {
        // intent: `--path docs/foo.md` and `--path ./docs/foo.md` must collapse to one resource.
        assert_eq!(
            normalize_file_resource("docs/foo.md", &workdir()),
            "file:docs/foo.md"
        );
        assert_eq!(
            normalize_file_resource("./docs/foo.md", &workdir()),
            "file:docs/foo.md"
        );
    }

    #[test]
    fn backslashes_become_forward_slashes() {
        // intent: Windows-style separators normalize to POSIX so cross-platform agents conflict-detect.
        assert_eq!(
            normalize_file_resource("docs\\foo.md", &workdir()),
            "file:docs/foo.md"
        );
    }

    #[test]
    fn relative_path_with_nested_dot_segments() {
        // intent: redundant `./` segments anywhere in the path are collapsed.
        assert_eq!(
            normalize_file_resource("./a/./b/c.md", &workdir()),
            "file:a/b/c.md"
        );
    }

    #[test]
    fn absolute_path_outside_workdir_stays_absolute() {
        // intent: paths the user explicitly anchored outside the repo are not silently rebased.
        let out = normalize_file_resource("/etc/hosts", &workdir());
        assert!(
            out == "file:/etc/hosts" || out.starts_with("file:/"),
            "got {out:?}"
        );
    }

    #[test]
    fn bare_dot_becomes_empty_resource() {
        // intent: `--path .` means "the repo itself" — emit `file:` with no path, matching Python behavior.
        assert_eq!(normalize_file_resource(".", &workdir()), "file:");
    }
}
