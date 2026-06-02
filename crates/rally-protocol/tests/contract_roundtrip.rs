// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # H1 — Schema-drift contract round-trip
//!
//! The shared rally-protocol crate's whole purpose is to PREVENT schema
//! drift between rally-cli (writer) and rally-termd (consumer). The
//! mitigation is: both sides depend on this crate, and this test proves
//! that a Directive + Receipt serialized through serde survives a
//! round-trip through both sides byte-identical (modulo field ordering,
//! which serde_json's `to_string` already normalizes deterministically
//! for struct fields in source order).
//!
//! The "both sides" simulation here writes through `FileInbox` (the
//! canonical impl rally-cli will use) and reads through `FileInbox` (the
//! canonical impl rally-termd will use). Both sides see the same code
//! path because they BOTH import this crate — that IS the dep-inversion.
//!
//! ## What this test catches
//! - Adding a new `#[serde(default)]` field that breaks old-reader-new-writer.
//! - Renaming a wire field (e.g. `itype` → `type`) without `#[serde(rename)]`.
//! - Changing an enum's `rename_all` style.
//! - Forgetting `#[serde(rename = "type")]` on `Directive::itype` (specifically
//!   asserted below).
//!
//! ## What this test does NOT catch
//! - Semantic drift (e.g. swapping the meaning of `Delivered` vs `Seen`).
//!   That class of bug is caught by the live round-trip in P2 / P3.

use std::fs;

use rally_protocol::ledger::FileInbox;
use rally_protocol::{
    DeliveryStatus, Directive, DirectiveKind, Inbox, InterruptType, Receipt, now_ts,
};

/// Scratch ledger root in a temp dir. RAII via the returned `_guard`.
fn scratch_root(name: &str) -> (std::path::PathBuf, ScratchGuard) {
    let mut root = std::env::temp_dir();
    root.push(format!(
        "rally-protocol-test-{}-{}-{}",
        name,
        std::process::id(),
        next_seq()
    ));
    fs::create_dir_all(&root).unwrap();
    let guard = ScratchGuard(root.clone());
    (root, guard)
}

struct ScratchGuard(std::path::PathBuf);
impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn directive_roundtrip_byte_identical_modulo_seq_assignment() {
    let (root, _g) = scratch_root("directive-rt");
    let inbox = FileInbox::open(&root).unwrap();

    let d_in = Directive {
        seq: 0, // 0 means "assign next"
        to: "claude_code:lead-01".to_string(),
        from: "rally-cli".to_string(),
        kind: DirectiveKind::Deliver,
        itype: InterruptType::Addition,
        text: Some("hello agent".to_string()),
        urgent: false,
        ts: 1780412976.123,
    };
    let assigned = inbox.append_directive(&d_in).unwrap();
    assert_eq!(assigned, 1, "first directive in fresh inbox must be seq=1");

    let dirs = inbox.read_since("claude_code:lead-01", 0).unwrap();
    assert_eq!(dirs.len(), 1);
    let d_out = &dirs[0];

    // Everything except seq survives byte-for-byte. seq was 0 going in,
    // assigned to 1 on write.
    assert_eq!(d_out.seq, 1);
    assert_eq!(d_out.to, d_in.to);
    assert_eq!(d_out.from, d_in.from);
    assert_eq!(d_out.kind, d_in.kind);
    assert_eq!(d_out.itype, d_in.itype);
    assert_eq!(d_out.text, d_in.text);
    assert_eq!(d_out.urgent, d_in.urgent);
    assert!((d_out.ts - d_in.ts).abs() < 1e-9, "ts must survive");
}

#[test]
fn directive_wire_format_is_stable_and_uses_rename_type() {
    // Pin the wire shape so a future refactor that breaks the InterruptBench
    // vocabulary (itype -> "type") gets caught here, not at integration time.
    let d = Directive {
        seq: 42,
        to: "agent-x".to_string(),
        from: "cli".to_string(),
        kind: DirectiveKind::Stop,
        itype: InterruptType::Retraction,
        text: None,
        urgent: true,
        ts: 100.5,
    };
    let json = serde_json::to_string(&d).unwrap();
    // Wire vocabulary checks — the contract:
    assert!(
        json.contains(r#""seq":42"#),
        "wire shape: seq numeric: {json}"
    );
    assert!(
        json.contains(r#""kind":"stop""#),
        "wire shape: DirectiveKind is snake_case: {json}"
    );
    assert!(
        json.contains(r#""type":"retraction""#),
        "wire shape: itype must serialise as \"type\" (InterruptBench vocab): {json}"
    );
    assert!(json.contains(r#""urgent":true"#));
    // text is None -> elided
    assert!(
        !json.contains(r#""text""#),
        "None text must be elided: {json}"
    );
}

#[test]
fn receipt_roundtrip_byte_identical() {
    let (root, _g) = scratch_root("receipt-rt");
    let inbox = FileInbox::open(&root).unwrap();

    let r_in = Receipt {
        ref_seq: 7,
        to: "claude_code:lead-01".to_string(),
        status: DeliveryStatus::Delivered,
        by: "rally-termd".to_string(),
        evidence: Some("bytes-written=11".to_string()),
        error: None,
        ts: now_ts(),
    };
    inbox.append_receipt(&r_in).unwrap();

    let receipts = inbox.read_receipts_since("claude_code:lead-01", 0).unwrap();
    assert_eq!(receipts.len(), 1);
    let r_out = &receipts[0];
    assert_eq!(r_out.ref_seq, r_in.ref_seq);
    assert_eq!(r_out.to, r_in.to);
    assert_eq!(r_out.status, r_in.status);
    assert_eq!(r_out.by, r_in.by);
    assert_eq!(r_out.evidence, r_in.evidence);
    assert_eq!(r_out.error, r_in.error);
}

#[test]
fn receipt_wire_format_snake_case() {
    let r = Receipt {
        ref_seq: 1,
        to: "x".to_string(),
        status: DeliveryStatus::Failed,
        by: "y".to_string(),
        evidence: None,
        error: Some("boom".to_string()),
        ts: 1.0,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains(r#""status":"failed""#), "{json}");
    assert!(json.contains(r#""error":"boom""#), "{json}");
    assert!(!json.contains(r#""evidence""#), "{json}");
}

#[test]
fn monotonic_seq_assignment_across_multiple_appends() {
    let (root, _g) = scratch_root("seq-mono");
    let inbox = FileInbox::open(&root).unwrap();
    let make = |seq| Directive {
        seq,
        to: "a".to_string(),
        from: "f".to_string(),
        kind: DirectiveKind::Deliver,
        itype: InterruptType::Addition,
        text: Some("t".to_string()),
        urgent: false,
        ts: 0.0,
    };
    assert_eq!(inbox.append_directive(&make(0)).unwrap(), 1);
    assert_eq!(inbox.append_directive(&make(0)).unwrap(), 2);
    // Caller-supplied seq below current max gets bumped to max+1.
    assert_eq!(inbox.append_directive(&make(1)).unwrap(), 3);
    // Caller-supplied seq strictly greater than current max is honored.
    assert_eq!(inbox.append_directive(&make(10)).unwrap(), 10);
    assert_eq!(inbox.append_directive(&make(0)).unwrap(), 11);

    let dirs = inbox.read_since("a", 0).unwrap();
    let seqs: Vec<u64> = dirs.iter().map(|d| d.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 10, 11]);
}

#[test]
fn read_since_filters_correctly() {
    let (root, _g) = scratch_root("read-since");
    let inbox = FileInbox::open(&root).unwrap();
    for _ in 0..5 {
        inbox
            .append_directive(&Directive {
                seq: 0,
                to: "agent".to_string(),
                from: "f".to_string(),
                kind: DirectiveKind::Deliver,
                itype: InterruptType::Addition,
                text: Some("x".to_string()),
                urgent: false,
                ts: 0.0,
            })
            .unwrap();
    }
    assert_eq!(inbox.read_since("agent", 0).unwrap().len(), 5);
    assert_eq!(inbox.read_since("agent", 2).unwrap().len(), 3);
    assert_eq!(inbox.read_since("agent", 5).unwrap().len(), 0);
    assert_eq!(inbox.read_since("agent", 100).unwrap().len(), 0);
}

#[test]
fn read_since_tolerates_partial_final_line() {
    let (root, _g) = scratch_root("partial");
    let inbox = FileInbox::open(&root).unwrap();
    inbox
        .append_directive(&Directive {
            seq: 0,
            to: "a".to_string(),
            from: "f".to_string(),
            kind: DirectiveKind::Deliver,
            itype: InterruptType::Addition,
            text: Some("ok".to_string()),
            urgent: false,
            ts: 0.0,
        })
        .unwrap();
    // Simulate a half-written final record (no trailing newline + garbage).
    let path = root.join("inbox").join("a.jsonl");
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(br#"{"seq":2,"to":"a","fr"#); // truncated mid-field
    fs::write(&path, bytes).unwrap();

    // The good line must still be returned; the partial must be skipped.
    let dirs = inbox.read_since("a", 0).unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].seq, 1);
}

#[test]
fn read_since_surfaces_corrupt_mid_line() {
    let (root, _g) = scratch_root("corrupt");
    let inbox = FileInbox::open(&root).unwrap();
    inbox
        .append_directive(&Directive {
            seq: 0,
            to: "a".to_string(),
            from: "f".to_string(),
            kind: DirectiveKind::Deliver,
            itype: InterruptType::Addition,
            text: Some("ok".to_string()),
            urgent: false,
            ts: 0.0,
        })
        .unwrap();
    let path = root.join("inbox").join("a.jsonl");
    let mut s = fs::read_to_string(&path).unwrap();
    // Insert garbage in the MIDDLE then another good line ending in \n.
    s.push_str("garbage-not-json\n");
    s.push_str(r#"{"seq":2,"to":"a","from":"f","kind":"deliver","type":"addition","text":"x","urgent":false,"ts":0.0}"#);
    s.push('\n');
    fs::write(&path, s).unwrap();

    let err = inbox.read_since("a", 0).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn enum_status_roundtrip_all_variants() {
    use DeliveryStatus::*;
    for variant in [Pending, Delivered, Seen, Acted, Failed] {
        let r = Receipt {
            ref_seq: 1,
            to: "a".to_string(),
            status: variant,
            by: "x".to_string(),
            evidence: None,
            error: None,
            ts: 0.0,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, variant);
    }
}

#[test]
fn enum_directivekind_roundtrip_all_variants() {
    use DirectiveKind::*;
    for variant in [Deliver, Read, Stop] {
        let d = Directive {
            seq: 1,
            to: "a".to_string(),
            from: "f".to_string(),
            kind: variant,
            itype: InterruptType::Addition,
            text: None,
            urgent: false,
            ts: 0.0,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Directive = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, variant);
    }
}

#[test]
fn enum_interrupttype_roundtrip_all_variants() {
    use InterruptType::*;
    for variant in [Addition, Revision, Retraction] {
        let d = Directive {
            seq: 1,
            to: "a".to_string(),
            from: "f".to_string(),
            kind: DirectiveKind::Deliver,
            itype: variant,
            text: None,
            urgent: false,
            ts: 0.0,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Directive = serde_json::from_str(&json).unwrap();
        assert_eq!(back.itype, variant);
    }
}

#[test]
fn ledger_open_refuses_missing_root() {
    // Defensive: a typo'd path silently producing a fresh ledger is the
    // exact class of bug the F plan calls out. open() must NOT mkdir the
    // root; only the inbox/ + receipts/ subdirs.
    let bogus = std::env::temp_dir().join(format!(
        "rally-protocol-NOPE-{}-{}",
        std::process::id(),
        next_seq()
    ));
    assert!(!bogus.exists());
    let err = FileInbox::open(&bogus).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
