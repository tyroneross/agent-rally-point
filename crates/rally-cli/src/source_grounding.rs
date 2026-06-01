// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! #6 Artifact source-grounding (dropped-work detection).
//!
//! At `rally say claim` with file paths, we snapshot a content hash of each
//! claimed file that exists, storing it as `claimhash:<path>=<hash>` markers
//! in the claim fact's `evidence`.
//!
//! At `rally say artifact` closing a claim, we re-hash the same files. If a
//! file is byte-identical to its claim-open hash (no work was done), we:
//! - Set a `grounded:false` marker in the artifact fact's `scope`.
//! - Append a `risk` fact (severity=warn) noting the unchanged file.
//!
//! Never blocks. Advisory only.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Compute a lightweight hash of a file's contents using a simple FNV-1a
/// 64-bit fold. No external deps — zero allocations beyond the read buffer.
///
/// Returns `None` if the file does not exist or cannot be read.
pub(crate) fn hash_file(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let hash = fnv1a_64(&bytes);
    Some(format!("{hash:016x}"))
}

fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x00000100000001b3);
    }
    hash
}

/// Build `claimhash:<path>=<hash>` evidence markers for all provided
/// repo-relative file paths that currently exist on disk.
///
/// `repo_root` is the root directory used to resolve relative paths.
/// `file_paths` are already-normalised `file:…` scope entries from the claim.
pub(crate) fn claim_hashes(repo_root: &Path, file_paths: &[String]) -> Vec<String> {
    let mut markers = Vec::new();
    for raw in file_paths {
        // Strip the `file:` prefix if present to get a filesystem path.
        let rel = raw.strip_prefix("file:").unwrap_or(raw.as_str());
        let abs = if Path::new(rel).is_absolute() {
            Path::new(rel).to_path_buf()
        } else {
            repo_root.join(rel)
        };
        if let Some(hash) = hash_file(&abs) {
            markers.push(format!("claimhash:{rel}={hash}"));
        }
    }
    markers
}

/// Parse `claimhash:<path>=<hash>` entries from a claim fact's `evidence` vec.
/// Returns a map of `path → hash`.
pub(crate) fn parse_claim_hashes(evidence: &[String]) -> HashMap<String, String> {
    evidence
        .iter()
        .filter_map(|e| {
            let rest = e.strip_prefix("claimhash:")?;
            let (path, hash) = rest.split_once('=')?;
            Some((path.to_string(), hash.to_string()))
        })
        .collect()
}

/// Check whether any of the files from the original claim are byte-identical
/// to their claim-open hashes.
///
/// Returns a list of paths that appear unchanged — each is a candidate for
/// a `grounded:false` + `ungrounded-artifact` risk fact.
pub(crate) fn ungrounded_paths(
    repo_root: &Path,
    claim_hashes: &HashMap<String, String>,
) -> Vec<String> {
    claim_hashes
        .iter()
        .filter_map(|(rel, original_hash)| {
            let abs = if Path::new(rel.as_str()).is_absolute() {
                Path::new(rel.as_str()).to_path_buf()
            } else {
                repo_root.join(rel)
            };
            let current_hash = hash_file(&abs)?;
            if &current_hash == original_hash {
                Some(rel.clone())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rally-sg-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hash_file_returns_none_for_missing_file() {
        let dir = tmp_dir("hash-missing");
        assert!(hash_file(&dir.join("nonexistent.rs")).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_file_returns_stable_hash() {
        let dir = tmp_dir("hash-stable");
        let path = dir.join("f.rs");
        fs::write(&path, b"hello world").unwrap();
        let h1 = hash_file(&path).unwrap();
        let h2 = hash_file(&path).unwrap();
        assert_eq!(h1, h2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_file_changes_when_content_changes() {
        let dir = tmp_dir("hash-changes");
        let path = dir.join("f.rs");
        fs::write(&path, b"before").unwrap();
        let h1 = hash_file(&path).unwrap();
        fs::write(&path, b"after").unwrap();
        let h2 = hash_file(&path).unwrap();
        assert_ne!(h1, h2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claim_hashes_skips_missing_files() {
        let root = tmp_dir("claim-hashes-missing");
        let paths = vec!["file:no_such_file.rs".to_string()];
        let result = claim_hashes(&root, &paths);
        assert!(
            result.is_empty(),
            "missing file must produce no hash marker"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_hashes_produces_marker_for_existing_file() {
        let root = tmp_dir("claim-hashes-exists");
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        let paths = vec!["file:lib.rs".to_string()];
        let result = claim_hashes(&root, &paths);
        assert_eq!(result.len(), 1);
        assert!(result[0].starts_with("claimhash:lib.rs="));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_claim_hashes_round_trips() {
        let evidence = vec![
            "claimhash:src/lib.rs=0102030405060708".to_string(),
            "claimhash:src/main.rs=aabbccddeeff0011".to_string(),
            "something-else".to_string(),
        ];
        let parsed = parse_claim_hashes(&evidence);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["src/lib.rs"], "0102030405060708");
        assert_eq!(parsed["src/main.rs"], "aabbccddeeff0011");
    }

    #[test]
    fn ungrounded_paths_detects_unchanged_file() {
        let root = tmp_dir("ungrounded-unchanged");
        fs::write(root.join("a.rs"), b"static").unwrap();
        let original = hash_file(&root.join("a.rs")).unwrap();
        let hashes = [("a.rs".to_string(), original)].into();
        let result = ungrounded_paths(&root, &hashes);
        assert_eq!(result, vec!["a.rs".to_string()]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ungrounded_paths_does_not_flag_changed_file() {
        let root = tmp_dir("ungrounded-changed");
        fs::write(root.join("b.rs"), b"before").unwrap();
        let original = hash_file(&root.join("b.rs")).unwrap();
        fs::write(root.join("b.rs"), b"after change").unwrap();
        let hashes = [("b.rs".to_string(), original)].into();
        let result = ungrounded_paths(&root, &hashes);
        assert!(result.is_empty(), "changed file must not be flagged");
        fs::remove_dir_all(&root).ok();
    }
}
