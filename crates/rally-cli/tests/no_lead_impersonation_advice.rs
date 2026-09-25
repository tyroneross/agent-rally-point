// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
//! # ARP-R-01 hygiene — no surface tells an agent to act as someone else
//!
//! Identity in Rally is self-asserted and unsigned: `--tool X` posts as X
//! because the caller typed X. So a command spelled `--tool <lead>` or
//! `--tool <owner>` is not a placeholder, it is a working impersonation
//! template, and every surface that prints one is teaching the spoof.
//!
//! ## Why this test walks the repo instead of extending the Rust denylist
//!
//! 667f548 added `BANNED_SUGGESTIONS` inside `check.rs`. That test grades the
//! strings `check.rs` produces, which is the right bar for those strings and
//! blind to everything else: the survivor it did not catch was in
//! `docs/AGENT-STATE-MODEL.md`, and the same advice can equally land in the
//! shell hook, a skill, or a README. An agent reads all of those the same way.
//! A grep-shaped check in a gate script would cover them, but only in the gate
//! — this belongs in `cargo test -p rally-cli`, which is the check every change
//! to this crate already runs.
//!
//! The two tests are complementary and both stay: `check.rs`'s denylist grades
//! generated text at the point of generation, this one grades authored text
//! across the tree.
//!
//! ## The rule
//!
//! `--tool` takes the id of whoever runs the command. A `--tool <placeholder>`
//! is legitimate when the placeholder names the READER (`<you>`, `$TOOL`,
//! `<SELF>`, a concrete example id) and is a defect when it names a third
//! party (`<lead>`, `<owner>`, `<holder>`, `<them>`).
//!
//! `rally lead handoff --tool <lead>` is NOT a violation and is not listed: the
//! actor there is the lead operating on its own title, which is the one case
//! where the lead's id is the caller's id.

use std::fs;
use std::path::{Path, PathBuf};

/// Placeholders that name somebody other than the reader. Matched only in the
/// `--tool <x>` position, so prose about the lead is untouched.
const THIRD_PARTY_PLACEHOLDERS: [&str; 6] = [
    "<lead>", "<owner>", "<holder>", "<them>", "<other>", "<peer>",
];

/// `rally lead` subcommands act on the caller's OWN title, so `--tool <lead>`
/// there is the lead naming itself. Keyed by the full command prefix rather
/// than by file, so the exemption cannot quietly widen.
const SELF_ACTING_LEAD_COMMANDS: [&str; 3] = [
    "rally lead handoff",
    "rally lead relinquish",
    "rally lead assign",
];

fn repo_root() -> PathBuf {
    // tests/ -> rally-cli -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above crates/rally-cli")
        .to_path_buf()
}

/// Shipped surfaces only, via `git ls-files`: the tree also holds
/// `.rally.bak-*` snapshots of old worktrees, and re-grading a frozen copy of
/// a defect that is already fixed on the live path is noise that would get the
/// test deleted. Untracked also means "not something a reader can reach".
fn tracked_text_files(root: &Path) -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("git ls-files");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|rel| root.join(rel))
        .filter(|path| is_text_surface(path))
        .collect()
}

fn is_text_surface(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "md" | "sh" | "mjs" | "js" | "ts" | "py" | "json" | "toml" | "yml" | "yaml")
    )
}

/// The literal escape, for the one legitimate case: a denylist whose VALUE is
/// the banned string. Spelled distinctly from the `ARP-R-01` rule id, which
/// appears all over the codebase in ordinary prose and would be an exemption
/// anyone could trip into by citing the rule.
const EXPLICIT_ALLOW_MARKER: &str = "ARP-R-01-ALLOW";

/// A Rust `//` line is read by the compiler and by whoever opens the file; it
/// is never printed to a colliding agent. The defect class is agent-VISIBLE
/// text, so commentary that names the banned shape in order to ban it is not
/// the defect. String literals in the same files are still graded, because
/// those are what get rendered.
fn is_rust_comment_line(path: &Path, line: &str) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("rs") && line.trim_start().starts_with("//")
}

/// True when this `--tool <third-party>` occurrence is a `rally lead` self-act.
/// Looks back over the same line, which is where the command verb lives.
fn is_self_acting_lead(line: &str, at: usize) -> bool {
    let before = &line[..at];
    SELF_ACTING_LEAD_COMMANDS
        .iter()
        .any(|cmd| before.contains(cmd))
}

#[test]
fn no_lead_impersonation_advice() {
    let root = repo_root();
    let files = tracked_text_files(&root);
    assert!(
        files.len() > 50,
        "walk found only {} files — the traversal is broken, not the repo clean",
        files.len()
    );

    // This test's own doc comment names the banned strings in order to explain
    // them; grading itself would make the explanation the violation.
    let self_path = PathBuf::from(file!());
    let mut violations = Vec::new();

    for file in &files {
        if file.ends_with(&self_path) {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            if is_rust_comment_line(file, line) || line.contains(EXPLICIT_ALLOW_MARKER) {
                continue;
            }
            for placeholder in THIRD_PARTY_PLACEHOLDERS {
                let needle = format!("--tool {placeholder}");
                let mut from = 0;
                while let Some(rel) = line[from..].find(&needle) {
                    let at = from + rel;
                    if !is_self_acting_lead(line, at) {
                        violations.push(format!(
                            "{}:{}: `{needle}` — `--tool` takes the CALLER's id; this hands the \
                             reader a working impersonation of a third party (ARP-R-01). Use \
                             `--tool <you>`, or name the third party in prose without a command.",
                            file.strip_prefix(&root).unwrap_or(file).display(),
                            lineno + 1,
                        ));
                    }
                    from = at + needle.len();
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "surfaces tell an agent to post as someone else:\n{}",
        violations.join("\n")
    );
}
