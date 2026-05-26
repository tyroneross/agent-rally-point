// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_protocol::sha256_hash;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelResolution {
    pub channel_dir: PathBuf,
    pub repo_id: String,
    pub repo_root: PathBuf,
    pub source: RepoIdSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoIdSource {
    GitRemote,
    GitRoot,
    WorkingDirectory,
}

#[derive(Debug)]
pub enum RepoError {
    MissingHome,
    Io(std::io::Error),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => write!(f, "HOME is required to resolve Rally channel"),
            Self::Io(err) => write!(f, "failed to resolve repository channel: {err}"),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<std::io::Error> for RepoError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn resolve_channel_dir(
    cwd: impl AsRef<Path>,
    home: Option<&Path>,
) -> Result<ChannelResolution, RepoError> {
    let cwd = cwd.as_ref();
    let home = home.ok_or(RepoError::MissingHome)?;
    let repo_root = find_repo_root(cwd).unwrap_or_else(|| canonical_or_original(cwd));
    let (source, identity) = git_remote_identity(&repo_root)
        .map(|remote| (RepoIdSource::GitRemote, remote))
        .unwrap_or_else(|| {
            if has_git_dir(&repo_root) {
                (RepoIdSource::GitRoot, repo_root.display().to_string())
            } else {
                (
                    RepoIdSource::WorkingDirectory,
                    canonical_or_original(cwd).display().to_string(),
                )
            }
        });
    let repo_id = repo_id("repo", &identity);
    Ok(ChannelResolution {
        channel_dir: home.join(".agent-rally-point/apps").join(&repo_id),
        repo_id,
        repo_root,
        source,
    })
}

fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(path) = current {
        if has_git_dir(path) {
            return Some(canonical_or_original(path));
        }
        current = path.parent();
    }
    None
}

fn has_git_dir(path: &Path) -> bool {
    path.join(".git").exists()
}

fn git_remote_identity(repo_root: &Path) -> Option<String> {
    let config = fs::read_to_string(git_config_path(repo_root)?).ok()?;
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == r#"[remote "origin"]"#;
            continue;
        }
        if in_origin {
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            if key.trim() == "url" {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn git_config_path(repo_root: &Path) -> Option<PathBuf> {
    let git = repo_root.join(".git");
    if git.is_dir() {
        return Some(git.join("config"));
    }
    let git_file = fs::read_to_string(git).ok()?;
    let git_dir = git_file.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = PathBuf::from(git_dir);
    Some(if git_dir.is_absolute() {
        git_dir.join("config")
    } else {
        repo_root.join(git_dir).join("config")
    })
}

fn repo_id(prefix: &str, value: &str) -> String {
    let hash = sha256_hash(value.as_bytes());
    format!("{prefix}_{}", &hash["sha256:".len().."sha256:".len() + 16])
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn git_remote_defines_stable_channel() {
        let repo = temp_dir("rally-repo-git");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(
            repo.join(".git/config"),
            "[remote \"origin\"]\n  url = git@github.com:tyroneross/agent-rally-point.git\n",
        )
        .unwrap();
        let nested = repo.join("crates/rally-cli");
        fs::create_dir_all(&nested).unwrap();
        let home = temp_dir("rally-repo-home");

        let resolved = resolve_channel_dir(&nested, Some(&home)).unwrap();
        fs::remove_dir_all(repo).unwrap();

        assert_eq!(resolved.source, RepoIdSource::GitRemote);
        assert!(resolved.channel_dir.starts_with(home));
        assert!(resolved.repo_id.starts_with("repo_"));
    }

    #[test]
    fn working_directory_falls_back_without_git() {
        let cwd = temp_dir("rally-repo-path");
        fs::create_dir_all(&cwd).unwrap();
        let home = temp_dir("rally-repo-path-home");

        let resolved = resolve_channel_dir(&cwd, Some(&home)).unwrap();
        fs::remove_dir_all(cwd).unwrap();

        assert_eq!(resolved.source, RepoIdSource::WorkingDirectory);
        assert!(resolved.channel_dir.starts_with(home));
    }
}
