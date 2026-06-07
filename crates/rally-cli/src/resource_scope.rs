use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceType {
    Workspace,
    Repo,
    File,
    Dir,
    Branch,
    Commit,
    Port,
    Process,
    Service,
    Task,
    CrossRepo,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessMode {
    Exclusive,
    SharedRead,
    Advisory,
    Namespace,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct ResourceScope {
    pub(crate) resource_type: ResourceType,
    pub(crate) identifier: String,
    pub(crate) access: AccessMode,
}

impl ResourceScope {
    pub(crate) fn parse_claim_scope(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() || is_lineage_or_marker_scope(raw) {
            return None;
        }

        let (access, rest) = parse_access_prefix(raw);
        let (kind, identifier) = rest.split_once(':')?;
        let resource_type = parse_resource_type(kind)?;
        let identifier = canonical_identifier(resource_type.clone(), identifier);
        if identifier.is_empty() {
            return None;
        }
        let access = access.unwrap_or_else(|| default_access(&resource_type));
        Some(Self {
            resource_type,
            identifier,
            access,
        })
    }

    pub(crate) fn canonical_key(&self) -> String {
        format!(
            "{}:{}",
            self.resource_type.as_str(),
            self.identifier.as_str()
        )
    }

    pub(crate) fn conflicts_with(&self, other: &Self) -> bool {
        if !access_modes_conflict(self.access, other.access) {
            return false;
        }
        resources_overlap(self, other)
    }
}

impl ResourceType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Repo => "repo",
            Self::File => "file",
            Self::Dir => "dir",
            Self::Branch => "branch",
            Self::Commit => "commit",
            Self::Port => "port",
            Self::Process => "process",
            Self::Service => "service",
            Self::Task => "task",
            Self::CrossRepo => "cross-repo",
        }
    }
}

fn parse_access_prefix(raw: &str) -> (Option<AccessMode>, &str) {
    let Some((prefix, rest)) = raw.split_once(':') else {
        return (None, raw);
    };
    let access = match prefix {
        "exclusive" => Some(AccessMode::Exclusive),
        "shared_read" | "shared-read" => Some(AccessMode::SharedRead),
        "advisory" => Some(AccessMode::Advisory),
        "namespace" => Some(AccessMode::Namespace),
        _ => None,
    };
    match access {
        Some(mode) => (Some(mode), rest),
        None => (None, raw),
    }
}

fn parse_resource_type(kind: &str) -> Option<ResourceType> {
    match kind {
        "workspace" => Some(ResourceType::Workspace),
        "repo" => Some(ResourceType::Repo),
        "file" => Some(ResourceType::File),
        "dir" => Some(ResourceType::Dir),
        "branch" => Some(ResourceType::Branch),
        "commit" => Some(ResourceType::Commit),
        "port" => Some(ResourceType::Port),
        "process" => Some(ResourceType::Process),
        "service" => Some(ResourceType::Service),
        "task" => Some(ResourceType::Task),
        "cross-repo" | "cross_repo" => Some(ResourceType::CrossRepo),
        _ => None,
    }
}

fn default_access(resource_type: &ResourceType) -> AccessMode {
    match resource_type {
        ResourceType::Dir | ResourceType::Repo | ResourceType::Workspace => AccessMode::Namespace,
        _ => AccessMode::Exclusive,
    }
}

fn canonical_identifier(resource_type: ResourceType, raw: &str) -> String {
    let raw = raw.trim();
    match resource_type {
        ResourceType::File | ResourceType::Dir => canonical_path(raw),
        ResourceType::Port => raw
            .parse::<u16>()
            .map(|port| port.to_string())
            .unwrap_or_else(|_| raw.to_string()),
        ResourceType::Commit => raw.to_ascii_lowercase(),
        ResourceType::CrossRepo => {
            let mut parts = raw
                .split('+')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            parts.sort();
            parts.dedup();
            parts.join("+")
        }
        _ => raw.to_string(),
    }
}

fn canonical_path(raw: &str) -> String {
    let raw = raw.strip_prefix("./").unwrap_or(raw);
    let path = Path::new(raw);
    let mut parts = Vec::<String>::new();
    let absolute = path.is_absolute();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn is_lineage_or_marker_scope(raw: &str) -> bool {
    raw == "external-intake"
        || raw == "backlog-item"
        || raw.starts_with("run:")
        || raw.starts_with("step:")
        || raw.starts_with("parent-step:")
        || raw.starts_with("owns:")
        || raw.starts_with("produces:")
        || raw.starts_with("depends:")
}

fn access_modes_conflict(a: AccessMode, b: AccessMode) -> bool {
    !matches!(
        (a, b),
        (AccessMode::Advisory, _)
            | (_, AccessMode::Advisory)
            | (AccessMode::SharedRead, AccessMode::SharedRead)
            | (AccessMode::SharedRead, AccessMode::Exclusive)
            | (AccessMode::Exclusive, AccessMode::SharedRead)
    )
}

fn resources_overlap(a: &ResourceScope, b: &ResourceScope) -> bool {
    if a.resource_type == b.resource_type && a.identifier == b.identifier {
        return true;
    }

    match (&a.resource_type, &b.resource_type) {
        (ResourceType::Workspace, _) | (_, ResourceType::Workspace) => true,
        (ResourceType::Repo, ResourceType::File | ResourceType::Dir)
        | (ResourceType::File | ResourceType::Dir, ResourceType::Repo) => true,
        (ResourceType::Dir, ResourceType::File) => path_contains(&a.identifier, &b.identifier),
        (ResourceType::File, ResourceType::Dir) => path_contains(&b.identifier, &a.identifier),
        (ResourceType::Dir, ResourceType::Dir) => {
            path_contains(&a.identifier, &b.identifier)
                || path_contains(&b.identifier, &a.identifier)
        }
        _ => false,
    }
}

fn path_contains(parent: &str, child: &str) -> bool {
    parent == child || child.starts_with(&format!("{parent}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_scope_canonicalizes_file_paths() {
        let scope = ResourceScope::parse_claim_scope("file:./src/../src/lib.rs").unwrap();
        assert_eq!(scope.resource_type, ResourceType::File);
        assert_eq!(scope.identifier, "src/lib.rs");
        assert_eq!(scope.access, AccessMode::Exclusive);
        assert_eq!(scope.canonical_key(), "file:src/lib.rs");
    }

    #[test]
    fn resource_scope_parses_access_prefixes() {
        let scope = ResourceScope::parse_claim_scope("shared_read:file:src/lib.rs").unwrap();
        assert_eq!(scope.access, AccessMode::SharedRead);
        assert_eq!(scope.canonical_key(), "file:src/lib.rs");

        let dir = ResourceScope::parse_claim_scope("dir:src").unwrap();
        assert_eq!(dir.access, AccessMode::Namespace);
    }

    #[test]
    fn resource_scope_ignores_lineage_markers() {
        assert!(ResourceScope::parse_claim_scope("run:abc").is_none());
        assert!(ResourceScope::parse_claim_scope("step:structured-scopes").is_none());
        assert!(ResourceScope::parse_claim_scope("external-intake").is_none());
    }

    #[test]
    fn resource_scope_conflicts_same_file_exclusive() {
        let a = ResourceScope::parse_claim_scope("file:src/lib.rs").unwrap();
        let b = ResourceScope::parse_claim_scope("file:./src/lib.rs").unwrap();
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn resource_scope_conflicts_parent_dir_and_child_file() {
        let dir = ResourceScope::parse_claim_scope("dir:src").unwrap();
        let file = ResourceScope::parse_claim_scope("file:src/lib.rs").unwrap();
        assert!(dir.conflicts_with(&file));
    }

    #[test]
    fn resource_scope_allows_shared_read_with_exclusive() {
        let reader = ResourceScope::parse_claim_scope("shared_read:file:src/lib.rs").unwrap();
        let writer = ResourceScope::parse_claim_scope("file:src/lib.rs").unwrap();
        assert!(!reader.conflicts_with(&writer));
    }

    #[test]
    fn resource_scope_allows_advisory_overlap() {
        let advisory = ResourceScope::parse_claim_scope("advisory:file:src/lib.rs").unwrap();
        let writer = ResourceScope::parse_claim_scope("file:src/lib.rs").unwrap();
        assert!(!advisory.conflicts_with(&writer));
    }
}
