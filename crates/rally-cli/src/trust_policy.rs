// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use rally_trust::{PublicKeyStore, TrustContext, TrustPolicy, load_trust_file};
use std::env;
use std::path::PathBuf;

pub(crate) struct LoadedTrust {
    pub(crate) keys: PublicKeyStore,
    pub(crate) policy: TrustPolicy,
    pub(crate) source: Option<String>,
}

pub(crate) fn load_optional_trust(
    trust_policy: Option<&PathBuf>,
    no_default_trust_policy: bool,
) -> Result<Option<LoadedTrust>, String> {
    let Some(path) = trust_policy.cloned().or_else(|| {
        (!no_default_trust_policy)
            .then(default_trust_policy_path)
            .flatten()
    }) else {
        return Ok(None);
    };
    if trust_policy.is_none() && !path.exists() {
        return Ok(None);
    }
    let TrustContext { keys, policy } =
        load_trust_file(&path).map_err(|err| format!("failed to load trust policy: {err}"))?;
    Ok(Some(LoadedTrust {
        keys,
        policy,
        source: Some(path.display().to_string()),
    }))
}

pub(crate) fn default_trust_policy_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".agent-rally-point/identity/trust.toml"))
}
