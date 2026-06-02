// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! Adversarial tests for the control-plane security review (2026-06-02).
//!
//! These are REAL behavior tests — no stubs. They drive the canonical
//! `FileInbox` writer/reader and assert the hardening actually fires:
//!   * SEC-003 — path-traversal / malformed agent ids rejected at the write
//!     boundary; filename construction stays under `inbox/`.
//!   * SEC-007 — `inbox/` + `receipts/` dirs are 0700, the `.jsonl` files 0600.
//!   * SEC-008 — `Directive.text` over the 64 KiB ceiling rejected on write;
//!     the reader is line-incremental and skips an over-long single line
//!     instead of buffering the whole file.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use rally_protocol::ledger::{validate_agent_id, FileInbox, MAX_DIRECTIVE_TEXT_BYTES};
use rally_protocol::{now_ts, Directive, DirectiveKind, Inbox, InterruptType};

fn scratch_root(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-ledger-sec-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn directive(to: &str, text: &str) -> Directive {
    Directive {
        seq: 0,
        to: to.to_string(),
        from: "claude_code:test".to_string(),
        kind: DirectiveKind::Deliver,
        itype: InterruptType::Addition,
        text: Some(text.to_string()),
        urgent: false,
        ts: now_ts(),
    }
}

// ---------------------------------------------------------------------------
// SEC-003 — path traversal / malformed ids
// ---------------------------------------------------------------------------

#[test]
fn sec003_traversal_target_rejected_on_write() {
    let root = scratch_root("traversal");
    let inbox = FileInbox::open(&root).unwrap();

    // The canonical attack from the review: a `to` that walks out of inbox/.
    let err = inbox
        .append_directive(&directive("../../etc/passwd", "pwn"))
        .expect_err("traversal target MUST be rejected, not written");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // Nothing escaped: no etc/passwd-ish file anywhere under (or above) inbox/.
    assert!(
        !root.join("inbox").join("..").join("..").join("etc").exists()
            || !root.join("../../etc/passwd").exists(),
        "no file may have been created outside inbox/"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn sec003_empty_and_dot_and_separator_ids_rejected() {
    for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "evil/../x", "with space"] {
        assert!(
            validate_agent_id(bad).is_err(),
            "agent id {bad:?} must be rejected"
        );
    }
    // Canonical rally ids pass.
    for ok in [
        "claude_code:lead-01",
        "rally-cli",
        "rally-termd:heartbeat",
        "codex:fleet-enforce-01",
    ] {
        assert!(validate_agent_id(ok).is_ok(), "agent id {ok:?} must pass");
    }
    // Over-length is rejected.
    assert!(validate_agent_id(&"a".repeat(129)).is_err());
}

// ---------------------------------------------------------------------------
// SEC-007 — private file modes
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn sec007_inbox_and_receipt_modes_are_private() {
    use std::os::unix::fs::PermissionsExt;
    use rally_protocol::{DeliveryStatus, Receipt};

    let root = scratch_root("modes");
    let inbox = FileInbox::open(&root).unwrap();

    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(&root.join("inbox")), 0o700, "inbox/ must be 0700");
    assert_eq!(mode(&root.join("receipts")), 0o700, "receipts/ must be 0700");

    inbox.append_directive(&directive("agent-a", "hi")).unwrap();
    assert_eq!(
        mode(&inbox.directives_path("agent-a")),
        0o600,
        "inbox/<agent>.jsonl must be 0600"
    );

    inbox
        .append_receipt(&Receipt {
            ref_seq: 1,
            to: "agent-a".to_string(),
            status: DeliveryStatus::Delivered,
            by: "rally-termd".to_string(),
            evidence: None,
            error: None,
            ts: now_ts(),
        })
        .unwrap();
    assert_eq!(
        mode(&inbox.receipts_path("agent-a")),
        0o600,
        "receipts/<agent>.jsonl must be 0600"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn sec007_preexisting_loose_dir_is_repaired() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch_root("repair");
    // Pre-create inbox/ world-readable (simulating a ledger from before the fix).
    let inbox_dir = root.join("inbox");
    std::fs::create_dir_all(&inbox_dir).unwrap();
    std::fs::set_permissions(&inbox_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    // open() must clamp it back to 0700.
    let _ = FileInbox::open(&root).unwrap();
    assert_eq!(
        std::fs::metadata(&inbox_dir).unwrap().permissions().mode() & 0o777,
        0o700,
        "open() must repair a loose inbox/ mode"
    );
    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// SEC-008 — payload ceiling + bounded reader
// ---------------------------------------------------------------------------

#[test]
fn sec008_oversize_text_rejected_on_write() {
    let root = scratch_root("oversize");
    let inbox = FileInbox::open(&root).unwrap();

    let big = "X".repeat(MAX_DIRECTIVE_TEXT_BYTES + 1);
    let err = inbox
        .append_directive(&directive("agent-a", &big))
        .expect_err("1MB+ text MUST be rejected on write");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // A 1 MiB payload (well past the ceiling) — the explicit review case.
    let one_mb = "Y".repeat(1024 * 1024);
    assert!(inbox.append_directive(&directive("agent-a", &one_mb)).is_err());

    // At-the-ceiling payload is accepted.
    let at = "Z".repeat(MAX_DIRECTIVE_TEXT_BYTES);
    assert!(inbox.append_directive(&directive("agent-a", &at)).is_ok());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn sec008_reader_skips_overlong_line_keeps_valid() {
    let root = scratch_root("bounded-read");
    let inbox = FileInbox::open(&root).unwrap();

    // Two legitimate directives via the writer (seq 1, 2).
    inbox.append_directive(&directive("agent-a", "first")).unwrap();

    // A hostile, over-long single line written DIRECTLY to the file (bypassing
    // the writer's cap) — the bounded reader must skip it without OOM-buffering
    // the whole thing, and still return the surrounding valid records.
    let path = inbox.directives_path("agent-a");
    let mut giant = String::from("{\"junk\":\"");
    giant.push_str(&"A".repeat(2 * 1024 * 1024));
    giant.push_str("\"}\n");
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(giant.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    inbox.append_directive(&directive("agent-a", "second")).unwrap();

    let got = inbox.read_since("agent-a", 0).unwrap();
    let texts: Vec<&str> = got.iter().filter_map(|d| d.text.as_deref()).collect();
    assert!(texts.contains(&"first"), "valid pre-line preserved: {texts:?}");
    assert!(texts.contains(&"second"), "valid post-line preserved: {texts:?}");
    // The giant junk line is NOT parsed into a directive.
    assert_eq!(got.len(), 2, "only the two valid directives returned");

    std::fs::remove_dir_all(&root).ok();
}
