// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `rally init` running in a **consumer repo** — any
//! repo that isn't agent-rally-point itself.
//!
//! Before the original fix, `rally init` hardcoded five (now six, see
//! ARP-R-07) doc paths specific to agent-rally-point's own documentation set
//! (`RALLY.md`, `dynamic-workflows/COORDINATION.md`,
//! `dynamic-workflows/PROTOCOL.md`, `docs/ORCHESTRATION.md`,
//! `docs/ANY-AGENT-ONBOARDING.md`, `docs/HANDOFFS-AND-LAUNCHING-AGENTS.md`)
//! and hard-errored if any of them was missing from the target worktree —
//! which is every repo except this one. Worse, `.rally/` was created
//! *before* that check, so a failed `rally init` left a partially-initialised
//! `.rally/` behind.
//!
//! ARP-R-07 closed two further defects in the same generated pointer block:
//! D1) the `rally room --json` pointer taught an untrusted sink (peer-
//! authored ledger data) with no caveat, unlike every other surface in this
//! project that touches that command; D2) the "Deeper docs" links were
//! unconditional, so every one of them was a dead link in a consumer repo
//! that (correctly) carries none of agent-rally-point's own doctrine docs.
//!
//! RC-072 closed a third defect on the same surface: RALLY.md's "Where State
//! Lives" contract calls `facts.db`, `rallyd.sock`, and the `*.owner.lock`
//! family gitignored, but nothing in the product ever wrote an ignore rule.
//! agent-rally-point's own root `.gitignore` carried one by hand, so the
//! defect was invisible in this repo and universal everywhere else: measured
//! at 56a6e39, `rally init` in a scratch repo left `git check-ignore
//! .rally/facts.db` at exit 1 (not ignored) for all eight documented paths.
//! `rally init` — and first-open auto-init, which is how `.rally/` actually
//! appears for most rooms — now writes `.rally/.gitignore`. The consumer's own
//! root `.gitignore` is never read or written.
//!
//! These tests prove: pointer docs are optional and selectively recorded, a
//! genuine init failure leaves no `.rally/` directory behind, the
//! `rally room --json` pointer always carries the untrusted-data caveat,
//! every "Deeper docs" link in the generated block resolves on disk (or is an
//! absolute URL) — never a dead link — and every derived/lock/cache artifact
//! a live room writes is ignored while the canonical ledger stays committable.
//!
//! # Conventions
//! Mirrors `worktree_gc.rs`'s `tmp_dir`/`init_repo` helpers and
//! `claims_refresh.rs`'s `env!("CARGO_BIN_EXE_rally")` binary-invocation
//! idiom — all tests operate on ephemeral git repos in the OS temp dir and
//! never touch the live agent-rally-point checkout.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;
use common::test_git_fixture::{fixture_git, fixture_git_command};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-init-consumer-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap_or(p)
}

/// Initialise a bare-minimum git repo: `git init -b main` + initial empty
/// commit. A consumer repo — deliberately carries none of
/// agent-rally-point's own doc pointers.
fn init_repo(root: &Path) {
    fixture_git(root, &["init", "-q", "-b", "main"]);
    fixture_git(root, &["commit", "--allow-empty", "-m", "initial"]);
}

fn run_rally(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("rally invocation")
}

fn manifest_json(root: &Path) -> Value {
    let manifest_path = root.join(".rally").join("manifest.json");
    let raw = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse manifest.json: {e}"))
}

/// The six pointer-doc manifest keys agent-rally-point knows about.
const ALL_POINTER_LABELS: &[&str] = &[
    "guide",
    "doctrine",
    "protocol",
    "board",
    "any_agent_onboarding",
    "handoffs",
];

/// Extract the fenced `<!-- rally:start --> ... <!-- rally:end -->` block
/// from a generated `CLAUDE.md`/`AGENTS.md`. Marker strings are duplicated
/// here (rather than imported from `rally_cli::init`) because this is an
/// integration test exercising the compiled binary's actual output, not the
/// crate's internal API.
fn pointer_block_of(doc_content: &str) -> String {
    const START: &str = "<!-- rally:start -->";
    const END: &str = "<!-- rally:end -->";
    let start = doc_content
        .find(START)
        .unwrap_or_else(|| panic!("no start marker in doc:\n{doc_content}"));
    let end = doc_content
        .find(END)
        .unwrap_or_else(|| panic!("no end marker in doc:\n{doc_content}"));
    doc_content[start..end + END.len()].to_string()
}

/// Manually scan for `[text](target)` markdown links. No regex dependency
/// needed (and none is added — see ARP-R-07 fix notes): this is a small,
/// bounded, one-pass scan over a block we already know is short.
fn markdown_links(block: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(open_rel) = block[i..].find('[') {
        let open = i + open_rel;
        let Some(close_rel) = block[open..].find(']') else {
            break;
        };
        let close = open + close_rel;
        if block[close + 1..].starts_with('(')
            && let Some(paren_close_rel) = block[close + 1..].find(')')
        {
            let paren_close = close + 1 + paren_close_rel;
            let text = block[open + 1..close].to_string();
            let target = block[close + 2..paren_close].to_string();
            out.push((text, target));
            i = paren_close + 1;
            continue;
        }
        i = close + 1;
    }
    out
}

/// `git check-ignore <rel>` against the fixture repo. True ⇒ some rule
/// ignores that path. Uses [`fixture_git_command`] rather than `fixture_git`
/// because exit 1 here means "not ignored", a legitimate answer this test
/// asks for on purpose — not a failed git invocation.
///
/// `git check-ignore` matches on the pathname, so the file does not need to
/// exist. That is what lets these tests name artifacts (`rallyd.sock`, a
/// crash-scratch tempfile) that only a running daemon or a crash produces.
fn is_ignored(root: &Path, rel: &str) -> bool {
    let out = fixture_git_command(root)
        .args(["check-ignore", "-q", rel])
        .output()
        .expect("git check-ignore invocation");
    match out.status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!(
            "git check-ignore {rel} errored (exit {other:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// Every path RALLY.md's "Where State Lives" section documents as gitignored,
/// plus the sibling forms of the same artifacts. The siblings are not padding:
/// a `facts.db` rule that misses `facts.db-wal` still lets a committed WAL
/// carry frames the ledger never saw.
const MUST_BE_IGNORED: &[&str] = &[
    // Documented in RALLY.md.
    ".rally/facts.db",
    ".rally/rallyd.sock",
    ".rally/rallyd.owner.lock",
    ".rally/mutation.lock",
    ".rally/cursors.json",
    ".rally/.reconcile-cache.json",
    ".rally/snapshot.cache.json",
    ".rally/claim-index.json",
    // Same artifacts, sibling forms.
    ".rally/facts.db-wal",
    ".rally/facts.db-shm",
    ".rally/facts.db.corrupt.1786158756577960000",
    ".rally/direct.owner.lock",
    ".rally/session-reservation.lock",
    ".rally/rallyd.sock.addr",
    ".rally/rallyd.pid",
    ".rally/rallyd.log",
    ".rally/watch-cursor.json",
    // Crash scratch from the write-temp-then-rename path in `init.rs`.
    ".rally/manifest.json.tmp-a1b2c3",
];

/// The `.rally/` paths that MUST stay committable. `log/` and `archive/` are
/// the canonical append-only record; `manifest.json` is what an agent landing
/// in a fresh clone reads to find the rally point; `.gitignore` is the rules
/// file itself, which has to travel with the clone to be worth writing.
const MUST_STAY_COMMITTABLE: &[&str] = &[
    ".rally/manifest.json",
    ".rally/.gitignore",
    ".rally/log/2026-08-15.jsonl",
    ".rally/log/index.json",
    ".rally/archive/2026-01-01.jsonl",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// RC-072: `rally init` must leave every documented derived/lock/cache path
/// ignored. Measured against 56a6e39 this fails on all eight documented
/// paths — nothing in the product wrote an ignore rule at all.
#[test]
fn init_ignores_every_derived_lock_and_cache_path() {
    let root = tmp_dir("ignore-derived");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        root.join(".rally").join(".gitignore").exists(),
        "rally init must write .rally/.gitignore"
    );

    for rel in MUST_BE_IGNORED {
        assert!(
            is_ignored(&root, rel),
            "{rel} is derived/lock/cache state and must be gitignored after `rally init`;\
             \n.rally/.gitignore:\n{}",
            fs::read_to_string(root.join(".rally").join(".gitignore")).unwrap_or_default()
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// The negative control for the rule above: an over-broad ignore that also
/// swallowed the ledger would pass every assertion in
/// `init_ignores_every_derived_lock_and_cache_path` while silently making the
/// canonical record uncommittable — the worse of the two failures.
#[test]
fn init_leaves_the_canonical_ledger_committable() {
    let root = tmp_dir("ignore-canonical");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for rel in MUST_STAY_COMMITTABLE {
        assert!(
            !is_ignored(&root, rel),
            "{rel} is canonical (or the rules file itself) and must stay committable;\
             \n.rally/.gitignore:\n{}",
            fs::read_to_string(root.join(".rally").join(".gitignore")).unwrap_or_default()
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// The durable control against the residual risk of the denylist shape
/// documented on `IGNORE_ENTRIES`: a derived artifact added to rally later
/// that nobody remembers to add to the list.
///
/// Rather than re-asserting a hardcoded list, this exercises a room the way an
/// agent does and then sweeps what actually landed on disk: every file under
/// `.rally/` that is not canonical must be ignored. A new cache file lands
/// here as a failing test, not as a surprise in someone's `git status`.
#[test]
fn every_artifact_a_live_room_writes_is_ignored_or_canonical() {
    let root = tmp_dir("ignore-sweep");
    init_repo(&root);

    // Exercise the paths that write to `.rally/`: enter the room, append a
    // fact, take a claim, run the before-write gate, and read the room back.
    for args in [
        vec!["init", "--json"],
        vec!["enter", "--tool", "claude_code:01", "--json"],
        vec![
            "say",
            "artifact",
            "--tool",
            "claude_code:01",
            "--subject",
            "rc-072 sweep probe",
            "--json",
        ],
        vec![
            "say",
            "claim",
            "--tool",
            "claude_code:01",
            "--subject",
            "rc-072 sweep probe",
            "--path",
            "README.md",
            "--json",
        ],
        vec![
            "check",
            "before-write",
            "--tool",
            "claude_code:01",
            "--path",
            "README.md",
            "--json",
        ],
        vec!["room", "--json"],
        vec!["next", "--tool", "claude_code:01", "--json"],
    ] {
        run_rally(&root, &args);
    }

    // Canonical by directory (contents travel with the repo) or by name.
    const CANONICAL_DIRS: &[&str] = &["log", "archive"];
    const CANONICAL_FILES: &[&str] = &[
        "manifest.json",
        ".gitignore",
        // Legacy R1 monolith: canonical while it exists, migrated into
        // `log/` on first open.
        "ledger.jsonl",
    ];

    let rally_dir = root.join(".rally");
    let mut swept = 0usize;
    let mut unignored = Vec::new();
    for entry in fs::read_dir(&rally_dir).expect("read .rally") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            assert!(
                CANONICAL_DIRS.contains(&name.as_str()),
                "unexpected directory .rally/{name} — classify it as canonical or add it to \
                 IGNORE_ENTRIES in src/init.rs"
            );
            continue;
        }
        if CANONICAL_FILES.contains(&name.as_str()) {
            continue;
        }
        swept += 1;
        if !is_ignored(&root, &format!(".rally/{name}")) {
            unignored.push(name);
        }
    }

    assert!(
        swept > 0,
        "the sweep found no non-canonical files under .rally/ — the exercise steps \
         above stopped producing derived state, so this test is no longer proving anything"
    );
    assert!(
        unignored.is_empty(),
        "derived state left committable after a live room session: {unignored:?}\n\
         Add each to IGNORE_ENTRIES in src/init.rs (or classify it as canonical here).\n\
         .rally/.gitignore:\n{}",
        fs::read_to_string(rally_dir.join(".gitignore")).unwrap_or_default()
    );

    fs::remove_dir_all(&root).ok();
}

/// First-open auto-init: `rally enter` creates `.rally/` far more often than
/// anyone runs `rally init`, so the ignore rules have to land on that path
/// too. Without this, the common case still commits its own `facts.db`.
#[test]
fn first_open_auto_init_writes_the_ignore_without_rally_init() {
    let root = tmp_dir("ignore-autoinit");
    init_repo(&root);

    // Deliberately never runs `rally init`.
    let out = run_rally(&root, &["enter", "--tool", "claude_code:01", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        root.join(".rally").join(".gitignore").exists(),
        "first-open auto-init must write .rally/.gitignore"
    );
    for rel in MUST_BE_IGNORED {
        assert!(
            is_ignored(&root, rel),
            "{rel} must be ignored after first-open auto-init (no `rally init` run)"
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// Re-running `rally init` must leave `.rally/.gitignore` byte-identical, and
/// rules a human added outside the managed markers must survive. A generator
/// that clobbers hand-written rules gets turned off by its users.
#[test]
fn ignore_file_is_idempotent_and_preserves_rules_outside_the_markers() {
    let root = tmp_dir("ignore-idempotent");
    init_repo(&root);
    run_rally(&root, &["init", "--json"]);

    let ignore_path = root.join(".rally").join(".gitignore");
    let first = fs::read_to_string(&ignore_path).unwrap();

    run_rally(&root, &["init", "--json"]);
    assert_eq!(
        first,
        fs::read_to_string(&ignore_path).unwrap(),
        "a second `rally init` must leave .rally/.gitignore byte-identical"
    );

    // A human adds their own rule below the managed block.
    fs::write(&ignore_path, format!("{first}\n# mine\nlocal-scratch/\n")).unwrap();
    run_rally(&root, &["init", "--json"]);
    let after = fs::read_to_string(&ignore_path).unwrap();
    assert!(
        after.contains("local-scratch/"),
        "rules outside the managed markers must survive a re-run:\n{after}"
    );
    assert_eq!(
        after.matches("# rally:ignore:start").count(),
        1,
        "exactly one managed block after a re-run:\n{after}"
    );
    assert_eq!(
        after.matches("# rally:ignore:end").count(),
        1,
        "exactly one managed block after a re-run:\n{after}"
    );
    // The managed rules still work with a user rule appended.
    assert!(is_ignored(&root, ".rally/facts.db"));

    fs::remove_dir_all(&root).ok();
}

/// Scope boundary: the consumer's root `.gitignore` belongs to the consumer.
/// rally writes a nested `.rally/.gitignore`, which needs no cooperation from
/// the repo owner, and must never create or edit the root file.
#[test]
fn rally_never_touches_the_repo_root_gitignore() {
    let root = tmp_dir("ignore-root-untouched");
    init_repo(&root);

    // (a) No root .gitignore to begin with — rally must not create one.
    run_rally(&root, &["init", "--json"]);
    run_rally(&root, &["enter", "--tool", "claude_code:01", "--json"]);
    assert!(
        !root.join(".gitignore").exists(),
        "rally must not create the consumer's root .gitignore"
    );

    // (b) A root .gitignore that already exists must come back byte-identical.
    let root_ignore = root.join(".gitignore");
    let original = "# the consumer's own rules\nnode_modules/\ndist/\n";
    fs::write(&root_ignore, original).unwrap();
    run_rally(&root, &["init", "--json"]);
    run_rally(&root, &["room", "--json"]);
    assert_eq!(
        original,
        fs::read_to_string(&root_ignore).unwrap(),
        "rally must not edit the consumer's root .gitignore"
    );

    fs::remove_dir_all(&root).ok();
}

/// `rally init` in a repo with NONE of agent-rally-point's five pointer docs
/// must exit 0 and produce a usable `.rally/manifest.json`. Before the fix
/// this hard-errored on the first missing doc (`docs.guide`).
#[test]
fn init_succeeds_in_a_repo_with_none_of_the_pointer_docs() {
    let root = tmp_dir("none");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "rally init must succeed in a repo with no pointer docs\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );

    let manifest = manifest_json(&root);
    assert!(
        manifest.get("repo").and_then(Value::as_str).is_some(),
        "manifest must carry repo: {manifest}"
    );
    assert_eq!(manifest["schema"], "agent-rally.manifest.v1");
    assert!(
        manifest.get("ledger").and_then(Value::as_str).is_some(),
        "manifest must carry ledger: {manifest}"
    );
    assert_eq!(manifest["room_cmd"], "rally room");
    assert_eq!(manifest["init_cmd"], "rally init");
    assert!(
        manifest.get("pointer_markers").is_some(),
        "manifest must carry pointer_markers: {manifest}"
    );

    fs::remove_dir_all(&root).ok();
}

/// The manifest's `docs` object must NOT contain a key whose pointer target
/// does not resolve. A consumer repo with none of the five docs must produce
/// an empty (or fully-absent-keyed) `docs` object.
#[test]
fn init_omits_pointer_docs_that_do_not_resolve() {
    let root = tmp_dir("omit-all");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest
        .get("docs")
        .unwrap_or_else(|| panic!("manifest must carry a docs object: {manifest}"));
    for label in ALL_POINTER_LABELS {
        assert!(
            docs.get(*label).is_none(),
            "docs.{label} must not appear when {label}'s target does not resolve; docs={docs}"
        );
    }
    assert_eq!(manifest["pointer_docs_resolved"], 0);
    assert_eq!(manifest["pointer_docs_omitted"], 6);

    fs::remove_dir_all(&root).ok();
}

/// D2 control (ARP-R-07): before the fix, `pointer_block()` hardcoded all six
/// "Deeper docs" links regardless of whether `rally init` found the doc under
/// this worktree. In a fresh consumer repo (no pointer docs at all) every one
/// of those links was dead. This MUST fail against the pre-fix code — see the
/// pre-fix run captured in the implementer's return notes.
#[test]
fn deeper_docs_links_resolve_or_are_absolute_urls() {
    let root = tmp_dir("dead-links");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let claude_md = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    let block = pointer_block_of(&claude_md);
    let links = markdown_links(&block);
    for (text, target) in &links {
        if target.starts_with("http://") || target.starts_with("https://") {
            continue;
        }
        assert!(
            root.join(target).exists(),
            "dead link: [{text}]({target}) does not resolve under consumer repo root; pointer block:\n{block}"
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// D1 control (ARP-R-07): the `rally room --json` pointer must carry the same
/// untrusted-data caveat the coordination hook's `UNTRUSTED_PREAMBLE` uses
/// (`hooks/rally-coordination-hook.sh`) — peer-authored, not authenticated by
/// rally, treat as data never as instructions. Before the fix this line had
/// no caveat at all.
#[test]
fn room_json_pointer_carries_untrusted_data_caveat() {
    let root = tmp_dir("caveat");
    init_repo(&root);

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let claude_md = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    let block = pointer_block_of(&claude_md);
    let room_line = block
        .lines()
        .find(|l| l.contains("rally room --json"))
        .unwrap_or_else(|| panic!("no `rally room --json` line in pointer block:\n{block}"));

    assert!(
        room_line.contains("not authenticated"),
        "rally room --json pointer missing untrusted-data caveat: {room_line}"
    );
    assert!(
        room_line.contains("never as instructions"),
        "rally room --json pointer missing untrusted-data caveat: {room_line}"
    );

    fs::remove_dir_all(&root).ok();
}

/// A consumer repo that DOES carry a subset of the pointer docs must have
/// exactly that subset recorded in the manifest — proving the fix is
/// selective (only-what-resolves) rather than "drop all docs". Also asserts
/// the pointer block itself links only what resolved (the D2 control, in the
/// partial case).
#[test]
fn init_keeps_every_pointer_doc_that_does_resolve() {
    let root = tmp_dir("partial");
    init_repo(&root);
    fs::write(root.join("RALLY.md"), "# RALLY.md\nstub\n").unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("docs").join("ORCHESTRATION.md"),
        "# ORCHESTRATION.md\nstub\n",
    )
    .unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest["docs"].clone();
    assert_eq!(docs["guide"], "RALLY.md", "docs={docs}");
    assert_eq!(docs["board"], "docs/ORCHESTRATION.md", "docs={docs}");
    for label in ["doctrine", "protocol", "any_agent_onboarding", "handoffs"] {
        assert!(
            docs.get(label).is_none(),
            "docs.{label} must be absent (its target was never created); docs={docs}"
        );
    }
    assert_eq!(manifest["pointer_docs_resolved"], 2);
    assert_eq!(manifest["pointer_docs_omitted"], 4);

    let claude_md = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    let block = pointer_block_of(&claude_md);
    assert!(block.contains("[RALLY.md](RALLY.md)"));
    assert!(block.contains("[docs/ORCHESTRATION.md](docs/ORCHESTRATION.md)"));
    assert!(!block.contains("dynamic-workflows/COORDINATION.md"));
    assert!(!block.contains("dynamic-workflows/PROTOCOL.md"));
    assert!(!block.contains("ANY-AGENT-ONBOARDING.md"));
    assert!(!block.contains("HANDOFFS-AND-LAUNCHING-AGENTS.md"));

    fs::remove_dir_all(&root).ok();
}

/// If `rally init` fails partway through — after `.rally/` has already been
/// created for the manifest write, but before the pointer-doc step
/// completes — it must not leave `.rally/` behind. We force a reproducible
/// failure by pre-creating `CLAUDE.md` as a DIRECTORY (not a file): the
/// manifest write succeeds first (docs are optional, so nothing there
/// fails), then the pointer-doc upsert step tries to `read_to_string` a
/// directory and errors. This is deterministic on any OS/user (unlike a
/// permissions-based failure, which silently no-ops when run as root).
#[test]
fn failed_init_leaves_no_rally_directory() {
    let root = tmp_dir("forced-failure");
    init_repo(&root);
    // Force upsert_pointer_in_doc("CLAUDE.md") to fail: it's a directory,
    // not a file, so `fs::read_to_string` on it returns an I/O error.
    fs::create_dir_all(root.join("CLAUDE.md")).unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        !out.status.success(),
        "rally init must fail when CLAUDE.md is a directory\nstdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );

    assert!(
        !root.join(".rally").exists(),
        "a failed rally init must not leave `.rally/` behind"
    );

    fs::remove_dir_all(&root).ok();
}

/// Sanity + D2 negative control (ARP-R-07): agent-rally-point's own repo
/// layout (all six docs present) still gets full-fidelity manifest entries
/// AND every "Deeper docs" link in the generated pointer block — this fix
/// must not regress the happy path or strip links a repo genuinely earned.
/// Simulated here rather than run against the live checkout (never touch the
/// live `.rally/`): a scratch repo seeded with all six docs must behave
/// identically to agent-rally-point's own tree.
#[test]
fn init_records_all_six_docs_when_all_six_are_present() {
    let root = tmp_dir("full-fidelity");
    init_repo(&root);
    fs::write(root.join("RALLY.md"), "# RALLY.md\n").unwrap();
    fs::create_dir_all(root.join("dynamic-workflows")).unwrap();
    fs::write(
        root.join("dynamic-workflows").join("COORDINATION.md"),
        "# COORDINATION.md\n",
    )
    .unwrap();
    fs::write(
        root.join("dynamic-workflows").join("PROTOCOL.md"),
        "# PROTOCOL.md\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("docs").join("ORCHESTRATION.md"),
        "# ORCHESTRATION.md\n",
    )
    .unwrap();
    fs::write(
        root.join("docs").join("ANY-AGENT-ONBOARDING.md"),
        "# ANY-AGENT-ONBOARDING.md\n",
    )
    .unwrap();
    fs::write(
        root.join("docs").join("HANDOFFS-AND-LAUNCHING-AGENTS.md"),
        "# HANDOFFS-AND-LAUNCHING-AGENTS.md\n",
    )
    .unwrap();

    let out = run_rally(&root, &["init", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let manifest = manifest_json(&root);
    let docs = manifest["docs"].clone();
    assert_eq!(docs["guide"], "RALLY.md");
    assert_eq!(docs["doctrine"], "dynamic-workflows/COORDINATION.md");
    assert_eq!(docs["protocol"], "dynamic-workflows/PROTOCOL.md");
    assert_eq!(docs["board"], "docs/ORCHESTRATION.md");
    assert_eq!(docs["any_agent_onboarding"], "docs/ANY-AGENT-ONBOARDING.md");
    assert_eq!(docs["handoffs"], "docs/HANDOFFS-AND-LAUNCHING-AGENTS.md");
    assert_eq!(manifest["pointer_docs_resolved"], 6);
    assert_eq!(manifest["pointer_docs_omitted"], 0);

    // Negative control: a repo carrying every doc must not lose any deeper-
    // docs link, and every one of them must resolve on disk.
    let claude_md = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    let block = pointer_block_of(&claude_md);
    let links = markdown_links(&block);
    assert_eq!(
        links.len(),
        6,
        "expected all six deeper-docs links: {block}"
    );
    for (text, target) in &links {
        assert!(
            root.join(target).exists(),
            "link [{text}]({target}) must resolve when its doc was seeded"
        );
    }

    fs::remove_dir_all(&root).ok();
}
