// SPDX-License-Identifier: Apache-2.0

//! Explicit recovery of one claim stranded by an exited local CLI process.
//! This never adopts the old identity or relaxes ordinary release authority.

use crate::error::{RallyError, Result};
use crate::store::{Fact, FactKind, RoomStore};
use schemars::JsonSchema;
use serde::Serialize;

fn local_host() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!host.is_empty()).then_some(host)
}

fn local_process_id(session: &str, host: &str) -> Option<i32> {
    let endpoint = session.strip_prefix("sess:proc:")?.strip_suffix("#live")?;
    let (owner_host, pid) = endpoint.rsplit_once(':')?;
    if !owner_host.eq_ignore_ascii_case(host) || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid: i32 = pid.parse().ok()?;
    (pid > 0).then_some(pid)
}

/// Called again by write authority under the mutation lock. Only ESRCH is
/// proof of absence: permission errors, live/reused PIDs and unknown hosts fail closed.
pub(crate) fn owner_process_is_dead(claim: &Fact) -> bool {
    let Some((host, session)) = local_host().zip(claim.from_session_id.as_deref()) else {
        return false;
    };
    let Some(pid) = local_process_id(session, &host) else {
        return false;
    };
    #[cfg(unix)]
    {
        // Signal zero probes existence without sending a signal to the owner.
        let result = unsafe { libc::kill(pid, 0) };
        result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct OrphanClaimReport {
    pub claim_id: String,
    pub owner_session: Option<String>,
    pub applied: bool,
}

pub(crate) fn recover(claim_id: &str, apply: bool) -> Result<OrphanClaimReport> {
    recover_in_room(&RoomStore::open()?, claim_id, apply)
}

fn recover_in_room(room: &RoomStore, claim_id: &str, apply: bool) -> Result<OrphanClaimReport> {
    let snapshot = room.snapshot()?;
    let claim = snapshot
        .active_claims
        .iter()
        .find(|claim| claim.event_id == claim_id);
    let Some(claim) = claim else {
        return Err(RallyError::Usage(format!(
            "no active claim with id {claim_id}"
        )));
    };
    if !owner_process_is_dead(claim) {
        return Err(RallyError::Usage(
            "orphan recovery refused: owner must be an absent process on this host; live, foreign, managed and unknown owners are preserved".into(),
        ));
    }
    if apply {
        let actor = crate::SystemActor::invoking_process();
        let close = Fact {
            schema: crate::FACT_SCHEMA.into(),
            event_id: format!("orphan-close-{}", claim.event_id),
            thread_id: claim.thread_id.clone(),
            kind: FactKind::ClaimExpired,
            tool: actor.tool_field(),
            role: actor.role_field(),
            from_session_id: actor.session_field(),
            subject: format!(
                "operator recovery: exited local process for {}",
                claim.event_id
            ),
            scope: claim.scope.clone(),
            created_at: crate::now_string(),
            ref_id: Some(claim.event_id.clone()),
            evidence: vec![
                format!("reaper:ref_id={}", claim.event_id),
                "reaper:reason=owner-process-dead".into(),
                "reaper:observed=stale".into(),
                format!(
                    "reaper:owner={}",
                    claim.tool.as_deref().unwrap_or("unknown")
                ),
                format!(
                    "reaper:owner_session={}",
                    claim.from_session_id.as_deref().unwrap_or("legacy")
                ),
            ],
            ..Default::default()
        };
        let outcome = room.append_state_transition_verified(&close)?;
        crate::record_append_outcome(&outcome);
    }
    Ok(OrphanClaimReport {
        claim_id: claim_id.into(),
        owner_session: claim.from_session_id.clone(),
        applied: apply,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphan_identity_rejects_other_hosts_and_non_process_sessions() {
        assert_eq!(
            local_process_id("sess:proc:local:42#live", "local"),
            Some(42)
        );
        assert_eq!(
            local_process_id("sess:proc:local:42#live", "LOCAL"),
            Some(42)
        );
        for session in [
            "sess:proc:remote:42#live",
            "sess:parent:local:42#live",
            "sess:proc:local:0#live",
            "sess:proc:local:-1#live",
            "sess:proc:local:2147483648#live",
            "sess:proc:local:42:other#live",
        ] {
            assert_eq!(local_process_id(session, "local"), None, "{session}");
        }
    }

    #[test]
    fn orphan_probe_preserves_live_and_unknown_owners() {
        let host = local_host().unwrap();
        let claim = Fact {
            from_session_id: Some(format!("sess:proc:{host}:{}#live", std::process::id())),
            ..Default::default()
        };
        assert!(!owner_process_is_dead(&claim));
        assert!(!owner_process_is_dead(&Fact::default()));
    }

    #[test]
    fn orphan_probe_detects_a_reaped_child() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let claim = Fact {
            from_session_id: Some(format!("sess:proc:{}:{pid}#live", local_host().unwrap())),
            ..Default::default()
        };
        assert!(owner_process_is_dead(&claim));
    }

    #[test]
    fn orphan_recovery_previews_then_closes_only_the_exact_dead_claim() {
        let root = std::env::temp_dir().join(format!("rally-orphan-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let room = RoomStore::open_at(root.clone()).unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let host = local_host().unwrap();
        let claim = Fact {
            schema: crate::FACT_SCHEMA.into(),
            event_id: "orphan-target".into(),
            thread_id: "orphan-test".into(),
            kind: FactKind::Claim,
            tool: Some("codex:owner".into()),
            from_session_id: Some(format!("sess:proc:{host}:{pid}#live")),
            scope: vec!["file:target.rs".into()],
            subject: "test target".into(),
            created_at: crate::now_string(),
            evidence: vec![format!(
                "lease_expires_at:{}",
                (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339()
            )],
            ..Default::default()
        };
        room.append_fact(&claim).unwrap();
        let live = Fact {
            event_id: "live-target".into(),
            scope: vec!["file:keep.rs".into()],
            from_session_id: Some(format!("sess:proc:{host}:{}#live", std::process::id())),
            ..claim.clone()
        };
        room.append_fact(&live).unwrap();
        assert!(
            !recover_in_room(&room, "orphan-target", false)
                .unwrap()
                .applied
        );
        assert_eq!(room.snapshot().unwrap().active_claims.len(), 2);
        assert!(recover_in_room(&room, "live-target", true).is_err());
        assert_eq!(room.snapshot().unwrap().active_claims.len(), 2);
        assert!(
            recover_in_room(&room, "orphan-target", true)
                .unwrap()
                .applied
        );
        let remaining = room.snapshot().unwrap().active_claims;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].event_id, "live-target");
        assert!(recover_in_room(&room, "orphan-target", true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
