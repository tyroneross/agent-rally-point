// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Contract tests for `rally help frame` — the human-readable definition of
//! the runtime message frame that every injected Rally message carries.
//!
//! # Why this file exists
//!
//! The frame is the only thing a receiving agent sees before it decides
//! whether an inbound message may direct its behavior. On the wire it renders
//! as a compact bracketed prefix with abbreviated keys, e.g.
//!
//! ```text
//! [rally: UNVERIFIED SENDER codex:01a04f40-… | intent=request(declared)
//!  | control=no(derived) | actor=agent(claimed)
//!  | seat=participant@14654(observed_for_claim)
//!  | responsibility=implementer(asserted)
//!  | authority=not_required(derived_for_claim) | caller_session=…]
//! ```
//!
//! A receiver that misreads any one of those fields either obeys a message it
//! should have treated as advisory, or ignores one it was obliged to act on.
//! `rally help frame` is the reference that makes the frame self-describing,
//! so this file pins what that help topic must say.
//!
//! # The contract, in four parts
//!
//! 1. **Reachable with no room.** `rally help frame` succeeds from a directory
//!    that is neither a Rally repo nor a git repo, and initializes nothing.
//!    A receiver reads the frame *before* it has entered a room; help that
//!    requires a room cannot be read at the moment it is needed.
//! 2. **Exactly eight fields.** The topic defines `sender`, `intent`,
//!    `control-attempt`, `sender-type`, `room-position`, `responsibility`,
//!    `authority`, and `guide` — no more, no fewer. An undocumented ninth
//!    field is an unreviewed instruction channel.
//! 3. **Three facts per field.** Each field states where its value comes from
//!    (its source or assurance level), what it changes about the receiver's
//!    behavior, and what happens when it is absent or unrecognized.
//! 4. **Responsibility is inert.** `responsibility` is a *claimed* category
//!    that grants neither scope nor authority. This is the field most likely
//!    to be read as permission — a sender asserting
//!    `responsibility=implementer` has asserted a job title, not a right to
//!    write files or direct a peer.
//!
//! # Expected format (what the assertions parse)
//!
//! The output is parsed as a flat list of field sections. A **field heading**
//! is a line indented four spaces or fewer whose first token is a lowercase
//! kebab-case word terminated by `:`, ` —`, or ` -`. A field's **section** is
//! its heading line plus every line up to the next heading. Prose indented
//! more than four spaces is body text and never reads as a heading, so
//! narrative paragraphs are free-form.
//!
//! # Conventions
//!
//! Follows `claims_refresh.rs`'s `env!("CARGO_BIN_EXE_rally")` invocation
//! idiom and `worktree_gc.rs`'s `tmp_dir` helper; strips ambient identity and
//! CI environment variables the way `claim_lifecycle_authority.rs` does, so a
//! developer's live session and CI both exercise the same code path. No
//! fixture ever touches the live agent-rally-point checkout.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The exact runtime frame fields `rally help frame` must define.
const FRAME_FIELDS: [&str; 8] = [
    "sender",
    "intent",
    "control-attempt",
    "sender-type",
    "room-position",
    "responsibility",
    "authority",
    "guide",
];

/// Any one of these words satisfies "this field states where its value comes
/// from" — either a named source or an assurance level.
const SOURCE_MARKERS: [&str; 11] = [
    "source",
    "assurance",
    "declared",
    "observed",
    "derived",
    "claimed",
    "asserted",
    "verified",
    "unverified",
    "attested",
    "self-reported",
];

/// Any one of these satisfies "this field states what it changes about the
/// receiver's behavior".
const EFFECT_MARKERS: [&str; 8] = [
    "effect",
    "behavio", // behavior / behaviour / behavioral
    "changes",
    "affects",
    "determines",
    "causes",
    "governs",
    "means",
];

/// Any one of these satisfies "this field states what happens when the value
/// is absent or unrecognized".
const UNKNOWN_MARKERS: [&str; 9] = [
    "unknown",
    "default",
    "absent",
    "missing",
    "omitted",
    "unset",
    "unrecognized",
    "fail closed",
    "fails closed",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh directory in the OS temp dir — outside any Rally repo and outside
/// any git worktree.
fn tmp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rally-framehelp-{label}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap_or(p)
}

/// Run `rally help frame` in `cwd` with ambient identity and CI variables
/// stripped. Returns (exit-success, combined stdout, stderr).
fn run_help_frame(cwd: &PathBuf) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rally"))
        .args(["help", "frame"])
        .current_dir(cwd)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_RUN_ID")
        .env_remove("RALLY_SESSION_ID")
        .env_remove("RALLY_OBSERVER_PID")
        .env_remove("RALLY_HOOK_SOURCE")
        .env_remove("TERM_SESSION_ID")
        .env_remove("TMUX_PANE")
        .env_remove("TTY")
        .env_remove("PWD")
        .output()
        .expect("failed to run `rally help frame`");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The frame help text, or a panic naming what went wrong.
fn frame_help_text(label: &str) -> String {
    let dir = tmp_dir(label);
    let (ok, stdout, stderr) = run_help_frame(&dir);
    assert!(
        ok,
        "`rally help frame` exited non-zero in {}\n--- stderr ---\n{stderr}",
        dir.display()
    );
    assert!(
        !stdout.trim().is_empty(),
        "`rally help frame` wrote nothing to stdout\n--- stderr ---\n{stderr}"
    );
    stdout
}

/// True when `line` is a field heading: indented four spaces or fewer, first
/// token a lowercase kebab-case word, terminated by `:`, ` —`, or ` -`.
/// Returns the field name.
fn heading_name(line: &str) -> Option<String> {
    let indent = line.len() - line.trim_start().len();
    if indent > 4 {
        return None;
    }
    let trimmed = line
        .trim_start()
        .trim_start_matches(['-', '*', '\u{2022}'])
        .trim_start();
    let token: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || *c == '-')
        .collect();
    if token.len() < 3 || token.starts_with('-') || token.ends_with('-') {
        return None;
    }
    let rest = &trimmed[token.len()..];
    let terminated = rest.starts_with(':')
        || rest.starts_with(" —")
        || rest.starts_with(" -")
        || rest.trim().is_empty();
    if terminated { Some(token) } else { None }
}

/// Split the help text into (field-name, section-body) pairs, lowercased.
fn field_sections(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        match heading_name(line) {
            Some(name) => sections.push((name, line.to_lowercase())),
            None => {
                if let Some(last) = sections.last_mut() {
                    last.1.push('\n');
                    last.1.push_str(&line.to_lowercase());
                }
            }
        }
    }
    sections
}

fn section_for<'a>(sections: &'a [(String, String)], field: &str) -> &'a str {
    sections
        .iter()
        .find(|(name, _)| name == field)
        .map(|(_, body)| body.as_str())
        .unwrap_or_else(|| {
            panic!(
                "`rally help frame` defines no section for the runtime frame field `{field}`; \
                 sections found: {:?}",
                sections.iter().map(|(n, _)| n).collect::<Vec<_>>()
            )
        })
}

fn mentions_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

// ---------------------------------------------------------------------------
// Part 1 — reachable with no room
// ---------------------------------------------------------------------------

/// A receiver reads the frame before it has entered a room. `rally help frame`
/// must therefore succeed with no `.rally/` and no git repo anywhere above the
/// working directory — and must not create either as a side effect.
#[test]
fn help_frame_succeeds_outside_a_rally_repo_and_initializes_nothing() {
    let dir = tmp_dir("outside");
    assert!(
        !dir.join(".rally").exists() && !dir.join(".git").exists(),
        "fixture precondition failed: {} is not a clean non-repo directory",
        dir.display()
    );

    let (ok, stdout, stderr) = run_help_frame(&dir);

    assert!(
        ok,
        "`rally help frame` must succeed outside a Rally repo, but exited non-zero.\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "`rally help frame` produced no stdout outside a Rally repo.\n--- stderr ---\n{stderr}"
    );
    assert!(
        !dir.join(".rally").exists(),
        "`rally help frame` created a room at {} — reading help must never \
         initialize state",
        dir.join(".rally").display()
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `rally help frame` must be its own topic, not a fallback to the general
/// usage dump. The general usage text names no frame field, so a run that
/// omits every one of them proves the topic is unimplemented rather than
/// merely differently worded.
#[test]
fn help_frame_is_a_distinct_topic_not_the_general_usage_dump() {
    let text = frame_help_text("distinct").to_lowercase();

    let named: Vec<&str> = FRAME_FIELDS
        .iter()
        .copied()
        .filter(|f| text.contains(*f))
        .collect();

    assert!(
        named.len() >= 4,
        "`rally help frame` looks like the general usage dump: it names only \
         {named:?} of the eight runtime frame fields {FRAME_FIELDS:?}.\n\
         --- output ---\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Part 2 — exactly eight fields
// ---------------------------------------------------------------------------

/// Every one of the eight runtime frame fields has its own section.
#[test]
fn help_frame_defines_all_eight_runtime_frame_fields() {
    let text = frame_help_text("all-fields");
    let sections = field_sections(&text);
    let defined: BTreeSet<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();

    let missing: Vec<&str> = FRAME_FIELDS
        .iter()
        .copied()
        .filter(|f| !defined.contains(*f))
        .collect();

    assert!(
        missing.is_empty(),
        "`rally help frame` defines no section for: {missing:?}\n\
         sections found: {defined:?}\n\
         A field heading is a line indented <=4 spaces whose first token is a \
         lowercase kebab-case word terminated by ':', ' —', or ' -'.\n\
         --- output ---\n{text}"
    );
}

/// No ninth field. An undocumented frame field is an instruction channel no
/// receiver has been taught to weigh, so the documented set must be closed.
#[test]
fn help_frame_defines_no_field_outside_the_documented_eight() {
    let text = frame_help_text("exact-set");
    let sections = field_sections(&text);
    let defined: BTreeSet<&str> = sections.iter().map(|(n, _)| n.as_str()).collect();

    // Guards against the check passing vacuously on output this parser cannot
    // read: eight headings must be found before "no extras" means anything.
    assert!(
        defined.len() >= FRAME_FIELDS.len(),
        "`rally help frame` yielded only {} parseable field headings ({defined:?}); \
         expected at least {}. Either a field is missing or the output does not use \
         the documented heading shape.\n--- output ---\n{text}",
        defined.len(),
        FRAME_FIELDS.len()
    );

    let extras: Vec<&str> = defined
        .iter()
        .copied()
        .filter(|f| !FRAME_FIELDS.contains(f))
        .collect();

    assert!(
        extras.is_empty(),
        "`rally help frame` documents runtime frame fields outside the agreed \
         eight: {extras:?}. Expected exactly {FRAME_FIELDS:?}.\n--- output ---\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Part 3 — three facts per field
// ---------------------------------------------------------------------------

/// Each field states where its value comes from. A receiver that cannot tell a
/// declared value from an observed one cannot tell a claim from a fact.
#[test]
fn every_field_states_its_source_or_assurance() {
    let text = frame_help_text("source");
    let sections = field_sections(&text);

    for field in FRAME_FIELDS {
        let body = section_for(&sections, field);
        assert!(
            mentions_any(body, &SOURCE_MARKERS),
            "frame field `{field}` does not say where its value comes from. \
             Expected one of {SOURCE_MARKERS:?} in its section.\n--- section ---\n{body}"
        );
    }
}

/// Each field states what it changes about the receiver's behavior. A field
/// with no stated effect is decoration, and a receiver will treat it as such.
#[test]
fn every_field_states_its_behavioral_effect() {
    let text = frame_help_text("effect");
    let sections = field_sections(&text);

    for field in FRAME_FIELDS {
        let body = section_for(&sections, field);
        assert!(
            mentions_any(body, &EFFECT_MARKERS),
            "frame field `{field}` does not say what it changes about the \
             receiver's behavior. Expected one of {EFFECT_MARKERS:?} in its \
             section.\n--- section ---\n{body}"
        );
    }
}

/// Each field states what happens when its value is absent or unrecognized.
/// Frames arrive from older senders and from senders that omit fields; the
/// receiver needs the fallback spelled out rather than inferred.
#[test]
fn every_field_states_unknown_or_default_handling() {
    let text = frame_help_text("unknown");
    let sections = field_sections(&text);

    for field in FRAME_FIELDS {
        let body = section_for(&sections, field);
        assert!(
            mentions_any(body, &UNKNOWN_MARKERS),
            "frame field `{field}` does not say how an unknown, absent, or \
             default value is handled. Expected one of {UNKNOWN_MARKERS:?} in \
             its section.\n--- section ---\n{body}"
        );
    }
}

// ---------------------------------------------------------------------------
// Part 4 — responsibility is inert
// ---------------------------------------------------------------------------

/// `responsibility` is the field most likely to be misread as permission.
/// The help must say it is claimed by the sender and that claiming it grants
/// neither scope nor authority.
#[test]
fn responsibility_is_a_claim_that_grants_neither_scope_nor_authority() {
    let text = frame_help_text("responsibility");
    let sections = field_sections(&text);
    let body = section_for(&sections, "responsibility");

    assert!(
        mentions_any(body, &["claim", "asserted", "self-reported"]),
        "the `responsibility` section does not present the value as a sender \
         claim.\n--- section ---\n{body}"
    );
    assert!(
        body.contains("scope"),
        "the `responsibility` section does not say it grants no scope.\n\
         --- section ---\n{body}"
    );
    assert!(
        body.contains("authority"),
        "the `responsibility` section does not say it grants no authority.\n\
         --- section ---\n{body}"
    );
    assert!(
        mentions_any(
            body,
            &[
                "neither",
                "does not grant",
                "grants no",
                "never grants",
                "not grant",
                "no authority"
            ]
        ),
        "the `responsibility` section mentions scope and authority but never \
         negates them. It must state that a claimed responsibility grants \
         NEITHER scope NOR authority.\n--- section ---\n{body}"
    );
}
