<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Plan: native before-write hook, option A — `rally hook before-write` owns the whole transaction

<!-- checklist
Item 1 — Auth guard: N/A: CLI + shell hook, no server routes. Trust boundary is SEC-001 (hooks/rally-coordination-hook.sh:592-707, unchanged) and ARP-004 (renderer sanitizers, ported to Rust).
Item 2 — External APIs: N/A: no new external API. Host hook contracts are the existing ones (Claude PreToolUse permissionDecision/systemMessage; Codex 0.142.5 rejects permissionDecision; Cursor preToolUse permission/agent_message; Gemini BeforeTool) — source of truth is the shell renderer at hooks/rally-coordination-hook.sh:2093-2147.
Item 3 — Rate-limit criterion: N/A: no paid API.
Item 4 — Discoverability: N/A: no UI. CLI discoverability = `rally --help` line for `hook` (lib.rs help_text test at lib.rs:9032 enforces).
Item 5 — Server/client boundary: N/A.
Item 6 — Concurrency: ledger appends go through RoomStore::append_fact_verified (mutation lock + read-back, store.rs:2911); dedupe state uses create_new/rename in <root>/.rally/.hook-events (mirrors shell mkdir-lock at hook.sh:1177-1234); once-markers use create_new (noclobber parity with hook.sh:330).
Item 7 — Observability: stderr diagnostics byte-identical to shell (unclassified/rejected/abort/claim-failed at hook.sh:331-336, 988, 1058); stdout abort advisory (fail-loud); marker files under .rally/.hook-seen; bench artifact docs/perf/*.json with as-of + build id.
Item 8 — Input validation: hook_runtime::parse_input + classify + normalize_targets port every check in the node classifier (hook.sh:169-297) and normalizer (hook.sh:356-433): trim, empty, >4096, control chars, ~, backslash, absolute-vs-cwd-relative for apply_patch, ≤16 targets, symlink/outside-root/equal-root rejection.
Item 9 — Stable ID traceability: U-01 → F-01..F-09 → D-01/D-02 → T-01..T-13; A-01..A-04. Every P0 row in the Spec Object carries a T- id.
Item 10 — JSON spec object: present, section "Spec Object (JSON)".
Item 11 — Blocking-and-novel gate: 2 open questions, each with blocking-test; everything else is [ASSUMED:] inline.
Item 12 — Low-reversibility ADRs: A-01 CLI surface (`rally hook <phase>`), A-02 host envelope contract, A-03 watchdog posture, A-04 probe/fallback. All linked from Locked Decisions.
Item 13 — Analytical lens: DSM (cross-component dependency: shell ↔ binary ↔ tests ↔ generated surfaces) + TRIZ for the "no rally spawn on reads" vs "classification moves into rally" contradiction.
Item 14 — Handoff document: N/A by operator instruction (single plan artifact); the Read-Before-Edit Map + Frozen Interface sections carry the "when implementing Cn read X, satisfy T-n" pointers.
Item 15 — Synthesis dimensions: N/A: no UI surface.
Item 16 — Risk reason: C1 `runtime protocol`; C3 `security boundary`; others none.
Item 17 — UI input/output contract: N/A: no UI surface.
Item 18 — Dispatch tier: declared per chunk (opus for C1, sonnet elsewhere).
Item 19 — Env-var manifest: N/A: no new external service. New env var RALLY_NATIVE_HOOK (on|off, default on) is documented in the shell header + docs/AUTO-COORDINATION-HOOKS.md.
Item 20 — Capability gap map: present.
Item 21 — Single-shot build guardrails: present.
Item 22 — Read-before-edit map: present.
-->

## Goal

One falsifiable sentence: **after this build, a mutation-shaped PreToolUse event in a Rally repo is served by exactly one `rally` process (no node, no perl, no per-call sub-budgets) that classifies, publishes working status, checks every target, auto-claims the unowned ones, dedupes, and renders the host envelope inside one self-enforced deadline — with p50 ≤ 100 ms (1 path) / ≤ 150 ms (4 paths) measured before/after on one build id, while every abort stays a stdout ADVISORY (never a deny) and every one of the 67 existing shell cases stays green in fallback mode.**

North star constraint (NORTH_STAR.md:23, invariant 4): rally advises, never gates. Commit 2a4cac0's `_rally_abort_envelope` (hook.sh:1010-1031) is the fail-loud contract this build carries into Rust, not a thing it replaces.

Worktree: `/Users/tyroneross/dev/git-folder/agent-rally-point/.build-loop/worktrees/native-hook-20260815` (branch `bl/native-before-write-2026-08-15`, base `main@96a431c`). All paths below are relative to that worktree.

## Locked Decisions

Analytical lens: DSM (dependency-structure across shell/binary/tests/generated surfaces) + TRIZ (contradiction: "reads spawn no Rally process" vs "classification moves into the Rust binary").

| # | Decision | Reversibility | ADR |
|---|----------|---------------|-----|
| L1 | Option A: `rally hook before-write` owns the WHOLE transaction in one process. Shell shrinks to opt-out → self-gate → SEC-001 → probe-once → `exec`; Node path is fallback only. Operator-decided; not re-litigated. | low | A-01 |
| L2 | New CLI surface `rally hook <phase> --tool T [--session-id S] [--repo-root DIR] [--strict]` (stdin host envelope → stdout host envelope, exit 0 always) + `rally hook capabilities --json` (standard `ok/product/command/data` envelope; the probe sentinel). Phase list additive (Path A below). | low | A-01 |
| L3 | Host envelope contract = the shell renderer's, byte-for-byte on keys/decisions: Claude allow+systemMessage / strict deny; Codex `systemMessage` ONLY (never `permissionDecision`, even strict); Cursor `permission`+`agent_message`; Gemini `additionalContext` / strict `decision:deny`. Abort → `{"systemMessage": advisory}` (Cursor: `{"permission":"allow","agent_message":advisory}`), never a permission field on Claude/Codex/Gemini. | low | A-02 |
| L4 | ONE deadline: `--timeout-ms` (global watchdog flag, already stripped/parsed at lib.rs:744-827) — inner stage checks via `watchdog_remaining()`, outer process watchdog gets a new `WatchdogPosture::HookAdvisory` that prints the SAME abort advisory. No perl, no per-call arithmetic. `RALLY_BEFORE_WRITE_FAILCLOSED` is NOT honored by `hook` (charter: hook advises). | medium | A-03 |
| L5 | Probe = `rally hook capabilities --json` executed once per (rally root, binary), cached in `<root>/.rally/.hook-seen/native-probe.<sanitized-bin-path>.seen` (valid while marker `-nt` binary), content `native` or `fallback`. `RALLY_NATIVE_HOOK=off` skips probe → Node fallback. Probe runs only AFTER SEC-001 containment on the same binary the hook would run anyway (ARP-001 unaffected: no download/chmod/build). | medium | A-04 |
| L6 | The 67-case shell suite and test_node_absence_advisory.sh export `RALLY_NATIVE_HOOK=off` in their headers: they are the FALLBACK-path suites (their stubs assert "zero Rally calls" on pure reads, which the exec probe would violate — see TRIZ note). The native path is proven by the 10 Rust goldens, which drive the shell hook end-to-end with the real debug binary. | high | — |
| L7 | Store opened ONCE per transaction (`RoomStore::open_at(root)`); per-path checks are `check::build_check` against ONE snapshot (in-memory); unowned filter reads the same snapshot's `active_claims`; writes = 1 working-status presence fact + 1 aggregate claim fact via `append_fact_verified`. Fact shapes mirror `command_status_post` (lib.rs:5075) and the claim branch of `command_say` (lib.rs:2819-2874, 2991-3024). | medium | — |
| L8 | Effect tables live in Rust as one-line `pub(crate) const` arrays and are pinned to `config/host-integrations.json` by extending the existing Python parity test (tests/scripts/test_generate_host_surfaces.py:47-68). config/host-integrations.json is NOT edited (peer-claimed). | high | — |
| L9 | No version bump, no CHANGELOG version header change (entry goes under existing `## Unreleased` body), no tag, no push. README.md and config/host-integrations.json untouched (peer `codex:release-cleanup-c5f8ebd7`); a merge step lists the exact README rows for the peer. | — | — |

## Scope

In scope: `rally hook before-write` + `rally hook capabilities`; hook_runtime.rs (parse, classify, normalize, identity, dedupe, deadline, render, abort, ARP-004 sanitizer port, transaction); watchdog posture; shell early-exec branch + probe; 10 goldens; suite headers; parity test; bench harness + before/after; docs + CHANGELOG body.

### Out of scope

- Lifecycle phases (`start`, `idle`, `after-write`) stay on the shell/Node path (Path A pay-it-forward only).
- Any change to config/host-integrations.json, README.md, plugin manifests, version, tags.
- Rewriting the Node fallback; it must stay byte-identical in behaviour (the 67 cases pin it).
- RC-037 auto-claim-failure stdout surfacing (stays stderr-only, parity) — recorded as follow-up F-10.
- `RALLY_HOOK_MS_BUDGET_SCALE` on the native path (no sub-budgets exist to scale; documented).

## Approach Lenses

**Clean-sheet.** The hook is `exec rally hook "$phase" "$tool"` for all four phases; the binary owns classification, ledger work, ARP-004 sanitizing, dedupe and rendering; node/perl are not dependencies; the store is opened once per fire; there is no probe because the binary is the hook.

**Current constraints.** (a) The lifecycle renderers are ~700 lines of node with byte-identical-block parity tests (tests/hooks/test_sanitizer_block_parity.sh, test_context_sanitization.sh `msg=` allowlist) — porting them is a second project. (b) 67 shell cases + test_node_absence_advisory.sh drive bash STUBS that assert "no Rally subprocess" on reads. (c) Installed binaries in the field may predate `hook` → the shell must detect and fall back. (d) config/host-integrations.json + README are peer-claimed. (e) `run_with_watchdog` (lib.rs:848) prints `{"ok":true,"product":"rally"}` on timeout — not a host envelope — so a bare `hook` command would fail-SILENT on a deadline miss.

**Bridge (this plan).** Native for before-write only, gated by a cached probe, Node retained as fallback; the CLI is shaped for all phases (`rally hook <phase>`, capabilities lists phases); the ARP-004 sanitizer is ported now (needed for the own-id `ident()` in the Next line, and it is the piece a later lifecycle port would otherwise re-derive); the watchdog grows a hook-specific advisory posture so the deadline miss is fail-loud on the channel the host reads.

TRIZ note (contradiction resolved by separation in space): the invariant "reads never become ownership / never write the ledger" is preserved in Rust (classify before store open); the cheaper "no Rally process on reads" was a cost heuristic for a ~40 ms node spawn and is superseded by a ~5 ms rally spawn that opens no store. docs/AUTO-COORDINATION-HOOKS.md:125-126 wording is updated in C5.

## Pay-it-forward — `rally hook` CLI surface (new long-lived public interface)

**Path A (chosen):** `rally hook <phase> --tool T [--session-id S] [--repo-root DIR] [--strict] [--json]` where `<phase>` is a positional subcommand (`before-write` now; `start|idle|after-write` are additive later). `rally hook capabilities --json` → `data.hook = {"contract":1,"phases":["before-write"],"native_effects":{pure_read,opaque_shell,mutation},"max_targets":16}`. The shell probes phase membership (`"before-write"` inside `phases`), so a future phase lands as: Rust arm + capabilities entry + one shell branch. Host family is derived from `--tool` prefix exactly as the shell does today (`codex|codex:*`, `cursor*`, `gemini*`, else Claude shape).

**Path B (rejected for v1, reserved):** explicit `--host claude_code|codex|cursor|gemini`. Cleaner for custom tool ids (`myagent:01` renders Claude-shaped today) but breaks parity with 67 pinned cases and the generated hook surfaces (which pass only the host family/tool). Reserve the flag name: `--host` MUST NOT be repurposed; when added it overrides prefix derivation.

`--json` on `hook before-write` is accepted and ignored (stdout is always the host envelope; wrappers that append `--json` must not break). `--timeout-ms`, `--fail-open`, `--fail-closed` are watchdog-level and stripped before parsing (lib.rs:800-827); `hook before-write` ignores `--fail-open`/`--fail-closed` for posture (always HookAdvisory).

## Frozen interface (C1 implements; C2–C5 build against it and MUST NOT renegotiate)

CLI (`crates/rally-cli/src/cli.rs`):
```rust
pub(crate) enum CliCommand { /* … existing … */ Hook(HookArgs) }
pub(crate) struct HookArgs { pub(crate) json: bool, pub(crate) subcommand: HookSubcommand }
pub(crate) enum HookSubcommand { BeforeWrite(HookBeforeWriteArgs), Capabilities }
pub(crate) struct HookBeforeWriteArgs {
    pub(crate) tool: String,                 // --tool (required)
    pub(crate) session_id: Option<String>,   // --session-id
    pub(crate) repo_root: Option<PathBuf>,   // --repo-root (SEC-001-validated rally root from the shell)
    pub(crate) strict: bool,                 // --strict; OR env RALLY_HOOK_STRICT=1
}
// COMMANDS gains "hook"; help_text() gains one line (lib.rs:9032 test).
```

`crates/rally-cli/src/hook_runtime.rs` (new; all `pub(crate)`):
```rust
pub(crate) const PURE_READ_TOOLS: &[&str]   = &["view_image","Read","Glob","Grep","WebFetch","WebSearch","read_file","list_dir","list_directory","codebase_search","grep_search"];
pub(crate) const OPAQUE_SHELL_TOOLS: &[&str] = &["exec_command","write_stdin","Bash","Shell","run_terminal_cmd"];
pub(crate) const MUTATION_TOOLS: &[&str]    = &["apply_patch","Write","Edit","MultiEdit","NotebookEdit","write_file","edit_file","delete_file","move_file","create_file","search_replace"];
// ^ each on ONE line, exact spelling/order of config/host-integrations.json hooks.native_effects; regex-pinned by C4.
pub(crate) const MAX_TARGETS: usize = 16;
pub(crate) const HOOK_CONTRACT_VERSION: u32 = 1;
pub(crate) const HOOK_PHASES: &[&str] = &["before-write"];
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub(crate) const STAGE_MARGIN_MS: u64 = 150;
pub(crate) const UNTRUSTED_PREAMBLE: &str = /* byte-identical to hook.sh:1346 */;

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum HostFamily { ClaudeCode, Codex, Cursor, Gemini }
impl HostFamily { pub(crate) fn from_tool(tool: &str) -> Self; pub(crate) fn event_name(self, phase: &str) -> &'static str; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum Carrier { Command, Legacy }
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect { PureRead, OpaqueShell, Mutation { carrier: Carrier }, Legacy, Unknown, Malformed { diagnostic: String } }

pub(crate) struct ParsedEnvelope { pub(crate) has_tool_name: bool, pub(crate) tool_name: Option<serde_json::Value>, pub(crate) session: String, pub(crate) cwd: Option<String>, pub(crate) tool_input: serde_json::Map<String, serde_json::Value> }
pub(crate) fn parse_input(raw: &str) -> Result<ParsedEnvelope, String>;               // Err(diagnostic) == malformed (hook.sh:205-226)
pub(crate) struct Classification { pub(crate) effect: Effect, pub(crate) tool: String, pub(crate) session: String, pub(crate) cwd: Option<String>, pub(crate) raw_paths: Vec<String> }
pub(crate) fn classify(env: &ParsedEnvelope) -> Classification;                        // pure port of hook.sh:228-297 (legacy aliases, apply_patch directives, validateTarget/uniqueValidated, MAX_TARGETS)
pub(crate) struct NormalizedTargets { pub(crate) cwd: PathBuf, pub(crate) paths: Vec<String> }
pub(crate) fn normalize_targets(root: &Path, cwd: Option<&str>, raw: &[String]) -> Result<NormalizedTargets, String>; // port of hook.sh:356-433 (physical ancestor walk, symlink realpath, no `..` after first missing, inside root, not root, '/'-joined, dedupe, ≤MAX_TARGETS)

pub(crate) fn id_segment(raw: &str) -> String;                                          // hook.sh:850-856
pub(crate) struct Identity { pub(crate) tool: String, pub(crate) session: String }
pub(crate) fn resolve_identity(argv_tool: &str, session_arg: Option<&str>, envelope_session: &str, env: &dyn Fn(&str) -> Option<String>, ppid: u32) -> Identity; // hook.sh:1103-1135 (RALLY_TOOL_ID > explicit "a:b" > host:RALLY_AGENT_ID|session; session: envelope > RALLY_SESSION_ID > TERM_SESSION_ID > TMUX_PANE > TTY > PPID > "<tool>-<epoch>")

pub(crate) struct Deadline { /* private */ }
impl Deadline { pub(crate) fn from_watchdog(default_ms: u64) -> Self; pub(crate) fn remaining(&self) -> Duration; pub(crate) fn exhausted(&self, margin: Duration) -> bool; }

#[derive(Clone, Debug)] pub(crate) struct Visible { pub(crate) severity: String, pub(crate) message: String }
#[derive(Clone, Debug)] pub(crate) struct PathJudgment { pub(crate) path: String, pub(crate) allow: bool, pub(crate) agent_visible: Option<Visible> }
#[derive(Clone, Debug)] pub(crate) struct AggregateCheck { pub(crate) allow: bool, pub(crate) targets: Vec<PathJudgment>, pub(crate) agent_visible: Option<Visible> }
pub(crate) fn aggregate_checks(judgments: Vec<PathJudgment>) -> AggregateCheck;        // hook.sh:896-930 ("target N: msg | …", max severity)
pub(crate) fn unowned_paths(active_claims: &[crate::store::Fact], tool: &str, paths: &[String]) -> Vec<String>; // hook.sh:944-976

#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum DedupeVerdict { Run, Suppress }
pub(crate) fn dedupe_event(state_dir: &Path, source: &str, session: &str, phase: &str, material: &str, window_secs: u64, now_epoch: u64) -> DedupeVerdict; // hook.sh:1145-1239 (per-source counts, max-count executes; unknown source → Run; sweeps >10 min files)

// ARP-004 boundary port (hook.sh:1344-1487): keep names.
pub(crate) fn line(v: &str, n: usize) -> String; pub(crate) fn scrub(v: &str) -> String; pub(crate) fn clip_id(s: &str, n: usize) -> String;
pub(crate) fn is_bare_shape(s: &str) -> bool; pub(crate) fn ident(v: &str, n: usize) -> String; pub(crate) fn host_id(v: &str, n: usize) -> String; pub(crate) fn prose(v: &str, n: usize) -> String;

pub(crate) fn render_before_write(host: HostFamily, tool: &str, check: Option<&AggregateCheck>, strict: bool) -> serde_json::Value; // {} when no visible signal; hook.sh:2013-2147 semantics
pub(crate) fn abort_advisory_text(tool: &str, reason: &str) -> String;                 // hook.sh:1019 text, [A-Za-z0-9 ._:-] reduced
pub(crate) fn abort_envelope(host: HostFamily, tool: &str, reason: &str) -> serde_json::Value; // hook.sh:1020-1030
pub(crate) fn abort_envelope_from_args(args: &[String], reason: &str) -> String;       // for the main-thread watchdog posture: derives --tool/RALLY_TOOL_ID → host + tool, returns compact JSON

pub(crate) struct HookRequest { pub(crate) tool_arg: String, pub(crate) session_arg: Option<String>, pub(crate) repo_root: Option<PathBuf>, pub(crate) strict: bool, pub(crate) stdin: String }
pub(crate) fn run_before_write(req: HookRequest) -> serde_json::Value;                 // NEVER Err; every failure path returns a host envelope ({} or advisory) and writes stderr/markers; exit code is always 0
pub(crate) fn capabilities() -> serde_json::Value;                                     // {"contract":1,"phases":[…],"native_effects":{…},"max_targets":16}
```

`crates/rally-cli/src/lib.rs`: `mod hook_runtime;` · `CliCommand::Hook(args) => command_hook(args)` · `fn command_hook(args: HookArgs) -> Result<Output>` (before-write → `Output::new(true, "hook before-write", hook_runtime::run_before_write(..))`, exit 0; capabilities → standard `envelope("hook", SCHEMA_HOOK, {"hook": …})`) · `WatchdogPosture::HookAdvisory` resolved FIRST in `resolve_watchdog_posture` (before the `fail_open` early return) when the first two positionals are `hook`,`before-write`; on timeout: `write_line_or_exit_on_broken_pipe(&hook_runtime::abort_envelope_from_args(&args, "coordination budget exceeded"))`, stderr note, `exit(0)` · debug-only seam `RALLY_TEST_HOOK_STAGE_BLOCK_MS` (sleep after snapshot, before the claim append; `#[cfg(debug_assertions)]`, same shape as `RALLY_TEST_BLOCK_MS` at lib.rs:1362).

Transaction order inside `run_before_write` (parity with hook.sh order): parse → classify → PureRead/OpaqueShell ⇒ `{}` immediately (no root, no store) → root (arg or walk up for `.rally`; none ⇒ `{}`) → `hooks_config::resolve(root)`; disabled ⇒ `{}` (parity: shell prints nothing; `{}` is host-valid) → Unknown/Malformed ⇒ once-marker + stderr diagnostic (byte-identical text) + `{}` → normalize (Mutation/Legacy) failure ⇒ malformed path → identity → dedupe (`RALLY_HOOK_SOURCE` ∈ plugin|project|global; state dir `RALLY_HOOK_DEDUPE_DIR` or `<root>/.rally/.hook-events`; Suppress ⇒ `{}`) → Deadline stage check → `RoomStore::open_at(root)` + `ensure_presence` → (mutation with ≥1 path) working-status presence fact (subject/evidence via `build_status_subject`/`presence_signal_evidence`, then `renew_owned_claim_leases`) → ONE snapshot (`snapshot_cache_capture(false)` + `write_snapshot_cache_for`) → `build_check("before-write", tool, Some(path), strict, &snapshot)` per path → `aggregate_checks` → if `allow` (no conflict): `unowned_paths` → claim fact (scope `file:<p>` via `scopes_from`, lease evidence via `claim_authority::ensure_lease_evidence` with `decay::classify_work_size`/`reclaim_timeout_secs`, `source_grounding::claim_hashes`, subject `auto-claim <first>` | `auto-claim <N> validated paths`) → `append_fact_verified`; failure ⇒ RC-037 stderr once per class → `render_before_write`. Legacy with 0 paths ⇒ unscoped check, no status, no claim. Deadline exhausted before any judgment ⇒ `abort_envelope`; exhausted after judgments with a conflict ⇒ render the conflict (no claim attempted); exhausted before the claim with no conflict ⇒ `abort_envelope(reason="auto-claim skipped (budget)")` + abort marker/stderr.

## Ordered chunks

Ownership is MECE by file; no file appears in two chunks. "Contract exposed" is what the next chunk consumes.

| # | Commit subject | files_touched | modifies_api | depends_on | dispatch_tier |
|---|----------------|---------------|--------------|------------|---------------|
| C0 | perf(hooks): before-write latency harness + baseline on 96a431c | `scripts/bench_hook_latency.py` (new), `docs/perf/2026-08-15-before-write-hook-latency.before.json` (new) | false | — | sonnet — bounded script + a measurement; no judgment beyond method |
| C1 | feat(cli): `rally hook before-write` owns the native transaction; `rally hook capabilities` | `crates/rally-cli/src/hook_runtime.rs` (new), `crates/rally-cli/src/cli.rs`, `crates/rally-cli/src/lib.rs` | true | C0 (baseline captured first) | opus — three-file change through the watchdog and store internals; a wrong shape here ripples into C2–C5 |
| C2 | test(cli): ten golden native before-write tests | `crates/rally-cli/tests/native_hook.rs` (new) | false | C1 (to pass; may be AUTHORED in parallel from the frozen contract) | sonnet — black-box process tests against a frozen contract |
| C3 | feat(hooks): before-write execs `rally hook`; Node path becomes fallback | `hooks/rally-coordination-hook.sh` | false | C1 (probe sentinel `"before-write"` in `data.hook.phases`) | sonnet — bounded shell edit with a pinned fallback |
| C4 | test(hooks): fallback-mode suite headers + Rust/JSON effect-table parity | `tests/hooks/test_rally_coordination_hook.sh`, `tests/hooks/test_node_absence_advisory.sh`, `tests/scripts/test_generate_host_surfaces.py` | false | C1 (const names), C3 (RALLY_NATIVE_HOOK env) | sonnet |
| C5 | docs+perf: after-measurement, docs, changelog body, peer merge note | `docs/perf/2026-08-15-before-write-hook-latency.after.json` (new), `docs/perf/2026-08-15-before-write-hook-latency.md` (new), `docs/AUTO-COORDINATION-HOOKS.md`, `CHANGELOG.md` | false | C1–C4 green | sonnet |

Integration checkpoints: after C1 — `cargo build -p rally-cli && cargo clippy -p rally-cli --all-targets -- -D warnings && cargo test -p rally-cli --lib` green; after C2+C3 — `cargo test -p rally-cli --test native_hook` green (10/10) AND `RALLY_NATIVE_HOOK=off bash tests/hooks/test_rally_coordination_hook.sh` green; after C4 — `scripts/check-release-parity.sh` exit 0; after C5 — `scripts/run-quality-gate.sh` exit 0, suite 5× consecutive under load.

### C0 — harness + baseline

- **What:** `scripts/bench_hook_latency.py` — creates a scratch repo under `/var/tmp` (`git init -q`, `mkdir .rally`, `rally enter --tool bench:peer --json`, one peer claim on `src/peer_only.rs`), then times `bash hooks/rally-coordination-hook.sh before-write <tool>` from Python (`time.perf_counter`, `subprocess.run` with the envelope on stdin), n ≥ 10 (default 20), scenarios: `claude_1path` (Write `src/a.rs`), `codex_4path` (apply_patch with 4 `*** Update File:` targets), `claude_pure_read`. Reports p50/p95/max/min per scenario, `os.getloadavg()` at start/end, `rally version --json` build id, hook sha, as-of. `--load N` spawns N `yes >/dev/null` burners for the duration and kills them (recorded PIDs, never `pkill -f`). Attribution: one traced fire per scenario via `bash -x` with `BASH_XTRACEFD` counting spawns of `node -e`, `perl -`, the resolved rally path, `$(`-subshells (approximate: count `++`-prefixed lines), plus measured unit startup costs (`node -e ""`, `perl -e 1`, `rally hook capabilities` / `rally version`, `bash -c :`) × counts.
- **Baseline run:** on the worktree HEAD BEFORE C1/C3 land, with the INSTALLED `rally` (record its build id) — write JSON to `docs/perf/2026-08-15-before-write-hook-latency.before.json`. Expect ≈570 ms p50 (operator's figure) — record what is measured, not the expectation.
- **Verification:** `python3 scripts/bench_hook_latency.py --repeat 20 --out docs/perf/2026-08-15-before-write-hook-latency.before.json` exits 0 and the JSON has `p50_ms`, `p95_ms`, `n`, `loadavg`, `build_id`, `spawns` per scenario. ✅ by running it.
- **Contract exposed:** JSON schema of the bench artifact (C5 reads it for the before/after table).

### C1 — Rust core (frozen interface above)

- **Recover donor first (read-only):** `git show f23b22a:crates/rally-cli/src/hook_runtime.rs > /tmp/donor_hook_runtime.rs`, `git show f23b22a:crates/rally-cli/tests/native_hook.rs`, `git show f23b22a -- crates/rally-cli/src/cli.rs crates/rally-cli/src/lib.rs hooks/rally-coordination-hook.sh`, and `git show f57056d` (why it was reverted: it contradicted the O33-A composite cdfcf86 — resolved: hook_runtime takes over classification + deadline). Reuse `parse_input`, `resolve_session/resolve_tool`, `host_family`, `render_before_write`, `duplicate_event`, `quote` where they match the frozen signatures; close the five known donor gaps: (1) multi-path ≤16, (2) O33-A classification (port the node tables/logic; the shell arrays at hook.sh:124-127 stay for the fallback), (3) `--timeout-ms` deadline + `HookAdvisory` posture, (4) Codex render = `{systemMessage}` only (VERIFIED against hook.sh:2117-2129: Codex PreToolUse emits `{systemMessage: message}` in both advisory and strict — donor already matches; keep), (5) fail-loud abort advisory (port hook.sh:1010-1031 text + hook.sh:978-989 marker/stderr).
- **Wiring:** `COMMANDS` += "hook"; `hook_parser()` (bpaf, style of `hooks_parser()` cli.rs:1262); dispatch arm; `command_hook`; `help_text()` line; `WatchdogPosture::HookAdvisory` (resolve first, ignore `--fail-open`); `is_fail_closed_mutation_invocation` unchanged (hook stays Open-family); `RALLY_TEST_HOOK_STAGE_BLOCK_MS` seam.
- **Do NOT** touch `command_say`/`command_status_post` bodies; build the two facts in hook_runtime with the same helpers (listed in Read-Before-Edit). Ensure `attach_pending_append_outcomes` cannot decorate the host envelope (it only inserts under `body.data`; the host envelope has no `data` key — keep it that way, and drain the collectors before returning).
- **Verification:** `cargo fmt --all -- --check && cargo clippy -p rally-cli --all-targets -- -D warnings && cargo test -p rally-cli --lib` green; manual smoke in a scratch repo: `printf '{"tool_name":"Read","tool_input":{"file_path":"x"}}' | target/debug/rally hook before-write --tool claude_code` prints `{}`; `target/debug/rally hook capabilities --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "before-write" in d["data"]["hook"]["phases"]'`; `RALLY_TEST_BLOCK_MS=1500 … --timeout-ms 300` prints the advisory within ~0.4 s.
- **risk_reason:** runtime protocol.
- **Contract exposed:** the CLI + stdout envelopes above; the capabilities sentinel; const names for C4's regex.

### C2 — ten goldens (`crates/rally-cli/tests/native_hook.rs`)

Fixture helper (in-file, not shared): scratch repo under `std::env::temp_dir()`; `fs::create_dir_all(repo/.git)` + `repo/.rally` (pattern of tests/json_envelope_contract.rs:19-46); `HOME` → scratch home; `RALLY_GLOBAL_INDEX=1`; `BIN = env!("CARGO_BIN_EXE_rally")`; `HOOK = <manifest_dir>/../../hooks/rally-coordination-hook.sh`; `ledger_lines(repo)` = total lines across `.rally/log/*.jsonl`; `active_claims(repo)` = `rally room --json` → `data.room.active_claims`. Seed peer: `rally enter --tool codex:peer --json` then `rally say claim --tool codex:peer --path src/shared.rs --subject peer --json`. Shell-driven cases pass `RALLY_BIN=BIN` (absolute, outside the scratch repo → SEC-001 accepts), `RALLY_SESSION_ID=<case>`, cwd = repo, envelope on stdin. Exactly these ten `#[test]` fns:

| T | Name | Drives | Asserts |
|---|------|--------|---------|
| T-01 | `claude_unclaimed_allows_and_autoclaims` | shell → binary, Write `src/new.rs`, tool `claude_code` | exit 0; stdout `{}` (allow-equivalent); `active_claims` gains one claim with tool `claude_code:<id_segment(session)>` and scope `file:src/new.rs`; one Presence fact with subject containing `working` and `src/new.rs`; probe marker exists with content `native` |
| T-02 | `claude_peer_claim_is_high_severity_advisory` | shell, Write `src/shared.rs` after seed | `hookSpecificOutput.permissionDecision=="allow"`, `hookEventName=="PreToolUse"`, `systemMessage` == `permissionDecisionReason`, contains `HIGH-SEVERITY`, `advisory — not blocking`, starts with `UNTRUSTED LEDGER DATA FOLLOWS`; no new claim by our tool; exit 0 |
| T-03 | `claude_strict_denies` | T-02 + `RALLY_HOOK_STRICT=1` | `permissionDecision=="deny"`, reason contains `STRICT MODE — BLOCKING`; no `systemMessage` key; exit 0 |
| T-04 | `codex_conflict_never_carries_permission_decision` | shell `before-write codex`, apply_patch (`tool_input.command` carrier) targeting `src/shared.rs`, cwd=repo; run twice: default and `RALLY_HOOK_STRICT=1` | stdout object keys == {`systemMessage`} exactly; message contains `HIGH-SEVERITY`; exit 0 both runs |
| T-05 | `pure_reads_return_immediately_without_ledger_work` | binary directly: Claude `Read`, `Glob`; Codex `read_file` | stdout `{}`; stderr empty; `ledger_lines` unchanged; recursive listing (name,len) of `.rally` unchanged; elapsed < 100 ms each |
| T-06 | `opaque_shell_and_target_cap` | binary: `Bash {"command":"rm -rf src; echo x > a.txt"}` and `exec_command`; then apply_patch with 16 `*** Add File:` → claim; 17 → malformed | opaque: `{}`, no ledger delta; 16: one claim with 16 `file:` scopes; 17: `{}`, stderr contains `exceeds 16 targets`, no ledger delta, marker `.rally/.hook-seen/<sess>.native-malformed-apply_patch.seen` exists |
| T-07 | `deadline_miss_is_fail_loud_advisory` | (a) shell with `RALLY_TEST_BLOCK_MS=1500 RALLY_HOOK_TIMEOUT_MS=300`; (b) binary with `RALLY_TEST_HOOK_STAGE_BLOCK_MS=800 --timeout-ms 400` | both: exit 0; elapsed < 1000 ms; stdout != `{}`; valid JSON; contains `rally coordination skipped` and `UNCLAIMED`; no `permissionDecision`, no `"decision"`; no claim landed. **Mutation check (record in commit body):** temporarily make `abort_envelope` return `{}` and `abort_envelope_from_args` return `"{}"` → T-07 must go RED (both sub-cases); revert |
| T-08 | `malformed_no_rally_or_old_binary_fail_open` | (a) binary, stdin `{not json`; (b) binary in a dir with no `.rally`; (c) shell with `RALLY_BIN=<bash stub lacking hook that logs argv to CALLS>` | (a) `{}`, one stderr diagnostic, no ledger delta; (b) `{}`, exit 0, no `.rally` created; (c) exit 0, marker content `fallback`, CALLS shows `hook capabilities` once then `hooks status` (Node path ran; requires node on PATH); scratch HOME listing unchanged |
| T-09 | `sec001_vectors_refuse_and_fall_back` | shell; planted executable = bash script that creates `<repo>/CANARY`; (i) `RALLY_BIN=<repo>/target/debug/rally`; (ii) `PATH=<repo>/bin:$PATH` with `bin/rally`; (iii) `RALLY_BIN=<outside>/rally` symlink → `<repo>/target/debug/rally` | each: exit 0; CANARY absent; stderr contains `SEC-001`; no probe marker naming the planted path |
| T-10 | `duplicate_event_runs_once` | binary; same envelope twice with `RALLY_HOOK_SOURCE=plugin` then `project` (window default 5 s), then a third with `plugin` again | after 2nd: `ledger_lines` delta 0 and stdout `{}`; after 3rd (same source, count exceeds executed): ledger grows again |

No other tests in this file. Verification: `cargo test -p rally-cli --test native_hook` 10 passed; run 3× to check flake-freedom. Env assumptions: `node`, `git`, `bash` on PATH for T-08c/T-09 (documented at top of file).

### C3 — shell early-exec branch

Insert immediately after the `RALLY_HOOKS` opt-out (hook.sh:134-136) and BEFORE `input="$(cat || true)"` (hook.sh:141-144):

1. `if [ "$phase" = "before-write" ]` and `RALLY_NATIVE_HOOK` not in `0|off|false|no|disabled` (case statement; no `tr` spawn) → `_rally_native_root="$(find_rally_root)"`.
2. `find_rally_root` rewritten with `${dir%/*}` instead of `dirname` (behaviour identical: walk to `/`; keep `pwd -P`).
3. Hoist the SEC-001 block (hook.sh:604-672: `_rally_repo_root`, `_rally_resolve_path`, `_rally_path_inside_repo`, RALLY_BIN validation, PATH containment, `~/.local/bin`) into `_rally_resolve_bin()` guarded by `_rally_bin_resolved=1`; both the native branch and the original site call it. Messages byte-identical (tests/hooks/test_no_autoprovision.sh:296-340 and the 67 cases grep them). Do not introduce a `msg=` assignment (test_context_sanitization.sh allowlist).
4. If a binary resolved (`command -v "$RALLY_BIN"` or `[ -x ]`) → `_rally_native_capable "$root" "$RALLY_BIN"`: marker `"$root/.rally/.hook-seen/native-probe.${RALLY_BIN//[^A-Za-z0-9._-]/_}.seen"`; if `[ -f "$marker" ] && [ "$marker" -nt "$RALLY_BIN" ]` → `read -r verdict < "$marker"`; else `out="$("$RALLY_BIN" hook capabilities --json 2>/dev/null || true)"`, verdict = `native` iff `$out` contains `"before-write"`, else `fallback`; write via tmp + `mv` (mkdir -p marker dir; failures → treat as fallback for this fire). Return 0 iff `native`.
5. Native → `exec "$RALLY_BIN" hook before-write --tool "$tool" --repo-root "$_rally_native_root" --timeout-ms "${RALLY_HOOK_TIMEOUT_MS:-3000}"` (stdin untouched; `RALLY_OBSERVER_PID` already exported at hook.sh:94). No `--fail-open`.
6. Otherwise fall through to the existing flow unchanged. Header comment: update the "NODE REQUIRED FOR HOOK OUTPUT" and "Env" blocks to describe `RALLY_NATIVE_HOOK` and the probe; nothing else in the Node path changes.

Verification: `bash -n hooks/rally-coordination-hook.sh`; `shellcheck hooks/rally-coordination-hook.sh` (if installed) no new findings; `RALLY_NATIVE_HOOK=off bash tests/hooks/test_rally_coordination_hook.sh` 67/67 (with C4's header, unset works too); with a scratch repo and the debug binary on `RALLY_BIN`: two consecutive fires show one probe exec (marker created once) — `bash -x` count of `hook capabilities` == 1 across both. risk_reason: security boundary. Contract exposed: `RALLY_NATIVE_HOOK`, marker path/format (T-01, T-08c, T-09 assert them).

### C4 — suites and parity

- `tests/hooks/test_rally_coordination_hook.sh` and `tests/hooks/test_node_absence_advisory.sh`: export `RALLY_NATIVE_HOOK="${RALLY_NATIVE_HOOK:-off}"` in the header with a comment: these suites drive bash stubs and pin the Node FALLBACK; the native path is `crates/rally-cli/tests/native_hook.rs`. No case bodies change; case count stays 67.
- `tests/scripts/test_generate_host_surfaces.py::test_native_effect_registry_matches_hook_classifier`: additionally regex `crates/rally-cli/src/hook_runtime.rs` for `^pub\(crate\) const (PURE_READ_TOOLS|OPAQUE_SHELL_TOOLS|MUTATION_TOOLS): &\[&str\] = &\[(.*)\];$` and assert the parsed list equals `registry[effect]` (same order, same case), and `MAX_TARGETS: usize = 16`.
- Verification: `python3 -m unittest tests/scripts/test_generate_host_surfaces.py`; `bash tests/hooks/test_rally_coordination_hook.sh` 5× consecutive under `--load` from C0's harness (or `yes` burners) all green; `bash tests/hooks/test_node_absence_advisory.sh` green; `scripts/check-release-parity.sh` exit 0.

### C5 — after-measurement, docs, changelog, peer note

- Build the debug/installed binary from the final SHA (`cargo build -p rally-cli --release`), run C0's harness with `RALLY_BIN=target/release/rally` → `.after.json`; write `docs/perf/2026-08-15-before-write-hook-latency.md` (before/after table: p50/p95 per scenario, load-avg, spawn attribution, build ids, invariant re-check list per .build-loop/goal.md criterion 12/14). Commit body of the integration commit carries the same table.
- `docs/AUTO-COORDINATION-HOOKS.md`: rows for `PreToolUse` (lines 125-128) gain the native sentence ("served by `rally hook before-write` when the installed binary reports it in `rally hook capabilities`; the shell/Node path is the fallback"), and the "wrapper classifies the native envelope before … any Rally subprocess" sentence (line 136-139) is scoped to fallback mode.
- `CHANGELOG.md` under `## Unreleased` (do not touch any version header): `### Changed — before-write coordination runs natively in the rally binary (option A)` with the measured before/after, the `RALLY_NATIVE_HOOK` switch, and the fail-loud contract carried over.
- Peer merge note (post to the room, not to files): README rows to update by `codex:release-cleanup-c5f8ebd7` — Optional tools/`node`: "Rendering lifecycle hook text and the before-write FALLBACK path; a `rally` build with `hook` needs no node for before-write" ; Opening-this-repo table `PreToolUse (edits)`: "Runs `rally hook before-write` (native) when available; advisory." config/host-integrations.json needs no change; if the peer's release lane changes the effect tables, C4's parity test fails until `hook_runtime.rs` consts are updated in lockstep.

## Capability Gap Map

| Capability/Workflow | Current source of truth | Target behavior | Gap | Build action | Owned files/contracts | Validation |
|---|---|---|---|---|---|---|
| Native before-write transaction | hooks/rally-coordination-hook.sh:1604-1691 (node×9, perl×5 subprocess chain) | one `rally hook before-write` process | no `hook` subcommand on main (reverted f57056d) | C1 | hook_runtime.rs, cli.rs, lib.rs | T-01..T-07 |
| O33-A classification | hook.sh:120-127, 150-303 (node) | Rust tables + classifier, ≤16 | donor had none | C1 + C4 parity | hook_runtime.rs consts | T-05, T-06, parity test |
| Deadline | per-call `rally_timeout_ms` arithmetic hook.sh:789-843, 1614-1620 | one `--timeout-ms`, inner stage checks + HookAdvisory posture | watchdog prints `{"ok":true}` on timeout (lib.rs:1146-1159) | C1 | lib.rs posture | T-07 (+ mutation check) |
| Fail-loud abort | hook.sh:1010-1031 (2a4cac0) | same advisory from Rust on both inner and outer deadline | donor had none | C1 | hook_runtime.rs | T-07 |
| Host envelopes | hook.sh:2093-2147 | byte-equal decisions/keys | donor Codex render `{systemMessage}` — already matches | C1 | render_before_write | T-02..T-04 |
| Dedupe | hook.sh:1145-1243 | same per-source semantics in Rust | donor `duplicate_event` single-source | C1 | dedupe_event | T-10 |
| Shell probe/exec/fallback | none | opt-out → self-gate → SEC-001 → probe-once → exec; else Node | — | C3 | hook.sh | T-01, T-08c, T-09, 67 cases (off) |
| Perf evidence | none for this change | before/after JSON + attribution | — | C0, C5 | scripts/bench_hook_latency.py, docs/perf/* | harness exit 0, numbers in commit body |

## Single-Shot Build Guardrails

| Guardrail | Prevents | Evidence/test |
|---|---|---|
| Codex PreToolUse output has exactly one key `systemMessage`, in strict mode too | Codex 0.142.5 "unsupported permissionDecision" rejection | T-04; hook.sh:2117-2129 |
| Abort/advisory envelopes carry no `permissionDecision`/`decision` on Claude/Codex/Gemini; Cursor uses `permission:"allow"` | gating or granting on an outage (charter) | T-07; hook.sh:1010-1031; existing shell case at test file :2500 |
| Pure reads and opaque shell never open the store or write markers | reads becoming ownership (O33-A) | T-05, T-06 |
| Effect tables in Rust equal config/host-integrations.json | classifier drift across surfaces | C4 parity test |
| Shell probe runs only after `_rally_resolve_bin` (SEC-001) and never on a path inside the scanned repo | attacker-supplied binary execution | T-09; tests/hooks/test_no_autoprovision.sh:296-340 |
| No new `msg=` in hook.sh; renderer blocks untouched | RC-040 allowlist / block-parity suites turning red | tests/hooks/test_context_sanitization.sh, test_sanitizer_block_parity.sh |
| `command_say`/`command_status_post` bodies unchanged; hook builds facts with the same helpers | claim/status shape drift + touching busy lib.rs regions | T-01 read-back (kind, tool, scope, lease evidence present) |
| `HookAdvisory` posture resolves before `fail_open` and only for (`hook`,`before-write`) | a stray `--fail-open` turning a deadline miss into `{"ok":true}` | T-07a |
| No `--fail-open` passed by the shell; no perl/timeout wrapper around the exec | double budgets, wrapper kill erasing the advisory | C3 diff review; T-07a elapsed bound |
| Suites export `RALLY_NATIVE_HOOK=off`; production default stays on | stub-driven cases asserting "no Rally call" going red under the probe | C4 header; T-01 asserts marker `native` under default env |
| No version bump; CHANGELOG entry only under `## Unreleased` body; README/host-integrations untouched | colliding with the release lane / peer claim | `git diff --stat` at integration lists none of those files |
| Bench numbers come from `scripts/bench_hook_latency.py` output with build id + as-of | unvetted performance claims (goal.md criterion 11/12) | docs/perf/*.json present and referenced in commit body |

## Read-Before-Edit Map

| Chunk | Read first | Why it matters | Edit after |
|---|---|---|---|
| C0 | hooks/rally-coordination-hook.sh:1-90 (usage/env), tests/hooks/test_rally_coordination_hook.sh:44-103 (`install_stub` warm-up rationale — pay first-exec cost outside the timed loop), CLAUDE.md "Performance claims" | measurement method must attribute, warm before timing | scripts/bench_hook_latency.py |
| C1 | donor via `git show f23b22a:…` and `git show f57056d`; hook.sh:120-303, 356-433, 850-856, 896-976, 978-1031, 1047-1063, 1103-1135, 1145-1243, 1604-1691, 1912-1923, 2013-2147; crates/rally-cli/src/lib.rs:315-362 (append outcome attach), 644-660 (watchdog consts), 744-827 (timeout flag), 848-1064 (watchdog + posture), 1352-1367 (test seam), 2244-2360 (`ensure_presence`, `renew_owned_claim_leases`, `presence_signal_evidence`), 2781-2874 + 2991-3026 (claim fact shape), 4956-5075 (`build_status_subject`, `validate_status_post_args`, `command_status_post`), 5563-5841 (`command_check` incl. cached-snapshot fast path), 9032-9050 (help-line test), 15207-15275 (`repo_root`), 16113 (`help_text`); crates/rally-cli/src/cli.rs:9-120, 793-852, 876-935, 1262-1285; hooks_config.rs:80-140; check.rs:47-140; claim_authority.rs:49-70; store.rs:2842-2946, 3036-3070, 9123-9135; output.rs:1-63; tests/json_envelope_contract.rs:1-101 | every helper the transaction reuses; watchdog output path; parity sources | hook_runtime.rs (new), cli.rs, lib.rs |
| C2 | this plan's Frozen interface + T-01..T-10 table; tests/json_envelope_contract.rs:19-72 (workspace pattern); test_rally_coordination_hook.sh:1093-1230, 1762-1870, 2500-2553 (assertion vocabulary to mirror) | goldens must mirror the pinned shell semantics | crates/rally-cli/tests/native_hook.rs |
| C3 | hook.sh:88-147, 461-556, 582-707 (SEC-001), 1010-1031; tests/hooks/test_no_autoprovision.sh:170-340; tests/hooks/test_context_sanitization.sh (grep `msg=`); tests/hooks/test_sanitizer_block_parity.sh | keep byte-identical messages/blocks; ordering of opt-out/self-gate | hooks/rally-coordination-hook.sh |
| C4 | tests/scripts/test_generate_host_surfaces.py:44-68; scripts/check-release-parity.sh:137-251 (which suites run) ; test suite headers :21-38 | one parity authority; suite semantics | the three test files |
| C5 | docs/AUTO-COORDINATION-HOOKS.md:119-149; CHANGELOG.md:1-12; .build-loop/goal.md criteria 11-14; C0/C5 JSON | evidence policy for numbers; wording that must not overclaim | docs + changelog |

## F-Criteria (functional)

| ID | Criterion | Pass condition | Falsifier | Grader |
|----|-----------|----------------|-----------|--------|
| F-01 | Single-process transaction | one `rally` spawn per mutation fire, no node/perl on the native path | `bash -x` trace shows `node -e`/`perl` on a native fire | C0 harness attribution |
| F-02 | O33-A parity | pure reads/opaque shell → `{}` no store; mutation ≤16 else malformed | T-05/T-06 red; parity test red | cargo test, unittest |
| F-03 | Host envelopes | Claude allow+systemMessage / strict deny; Codex `systemMessage` only; exit 0 | T-02..T-04 red | cargo test |
| F-04 | Auto-claim idempotent + unowned filter | claim only unowned paths, once; read-back shows scope | T-01/T-06 red; second identical fire creates a second claim | cargo test |
| F-05 | One deadline, fail-loud | both inner and outer deadline misses return the advisory within budget; mutation of the advisory turns T-07 red | T-07 stays green with the advisory removed | cargo test + recorded mutation run |
| F-06 | Dedupe | plugin+project same event ⇒ one ledger interaction | T-10 red | cargo test |
| F-07 | Fallback intact | 67/67 with `RALLY_NATIVE_HOOK=off`, 5× consecutive under load; probe→fallback when binary lacks `hook` | any case red; T-08c red | shell suite, cargo test |
| F-08 | SEC-001 | three vectors refused, planted binary never executed | T-09 red | cargo test |
| F-09 | Performance | p50 ≤100 ms (1 path), ≤150 ms (4 paths) unloaded; p95 ≤250 ms at load-avg ≥5; spawn count and output bytes not up | after.json above thresholds or attribution shows regression | C0/C5 harness |
| F-10 (follow-up, not this build) | RC-037 claim-failure on stdout | — | — | backlog item |

## Q-Criteria (quality)

| Criterion | Pass condition | Grader |
|-----------|----------------|--------|
| Rust | `cargo fmt --all -- --check`, `cargo clippy -p rally-cli --all-targets -- -D warnings`, `cargo test -p rally-cli` | run-quality-gate.sh |
| Parity gate | `scripts/check-release-parity.sh` exit 0 (host surfaces current, python tests, every tests/hooks suite) | script |
| Shell | `bash -n`; shellcheck no new findings | manual |
| Docs | no version header touched; README/host-integrations untouched | `git diff --stat` |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Perf target missed because `RoomStore::open_at` (router/ownership acquisition, store.rs:2886-2895) dominates | medium | store opened once (L7); the harness attributes; if p50 > target with store-dominated time, record honestly (goal.md: thresholds are reference points) and file the store-open cost as the next lever — do not add spawns or drop invariants to hit the number |
| Watchdog posture bypassed by `--fail-open` or by `RALLY_BEFORE_WRITE_FAILCLOSED` | low | posture resolved first; T-07a; hook ignores FAILCLOSED (documented) |
| Suites forced to `off` hide a native-mode regression the shell suite would have caught | medium | goldens drive the SHELL end-to-end for T-01..T-04, T-07a, T-08c, T-09; C5 re-runs the invariant probes from goal.md criterion 14 on the same build id |
| Probe exec on an OLD binary that lacks the internal watchdog hangs | low | old binaries print a usage error immediately (`reject_unknown_command`); the host's 10 s timeout is the last resort; the marker caches `fallback` so it happens once |
| `ident()`/preamble port drifts from the node renderer | medium | constants copied byte-for-byte; T-02 asserts preamble + severity phrases; ARP-R-08 shape rules ported with the same numeric bounds (64/2/4/3) |
| Peer's release lane changes effect tables → parity test red at merge | low | C4 test names the exact const lines to update; merge note to peer |
| `RALLY_TEST_*` seams leak into release | none | `#[cfg(debug_assertions)]` like the existing seam |

## Spec Object (JSON)

```json
{
  "needs": [{"id":"U-01","need":"an agent gets honest, fast before-write deconfliction on every host without node/perl, and can always tell an outage from a clean check","priority":"P0"}],
  "features": [
    {"id":"F-01","name":"single-process native transaction","need":"U-01","tests":["T-01","T-07"],"adr":"A-01"},
    {"id":"F-02","name":"O33-A classification in Rust","need":"U-01","tests":["T-05","T-06","T-12"]},
    {"id":"F-03","name":"host envelope parity","need":"U-01","tests":["T-02","T-03","T-04"],"adr":"A-02"},
    {"id":"F-04","name":"idempotent auto-claim","need":"U-01","tests":["T-01","T-06"]},
    {"id":"F-05","name":"one deadline, fail-loud advisory","need":"U-01","tests":["T-07"],"adr":"A-03"},
    {"id":"F-06","name":"dedupe","need":"U-01","tests":["T-10"]},
    {"id":"F-07","name":"probe + Node fallback","need":"U-01","tests":["T-08","T-11"],"adr":"A-04"},
    {"id":"F-08","name":"SEC-001 preserved","need":"U-01","tests":["T-09"]},
    {"id":"F-09","name":"measured latency improvement","need":"U-01","tests":["T-13"]}
  ],
  "data": [
    {"id":"D-01","name":"host envelope contract","shape":"Claude {hookSpecificOutput{hookEventName,permissionDecision,permissionDecisionReason},systemMessage?} | Codex {systemMessage} | Cursor {permission,agent_message} | Gemini {hookSpecificOutput{additionalContext}}|{decision,reason}; abort {systemMessage}|Cursor {permission:allow,agent_message}"},
    {"id":"D-02","name":"ledger facts per mutation fire","shape":"Presence(working, subject via build_status_subject) + Claim(scope file:<p>*, evidence lease+claimhash, subject auto-claim …)"},
    {"id":"D-03","name":"capabilities","shape":"data.hook={contract:1,phases:[before-write],native_effects:{…},max_targets:16}"}
  ],
  "tests": [
    {"id":"T-01","file":"crates/rally-cli/tests/native_hook.rs","name":"claude_unclaimed_allows_and_autoclaims"},
    {"id":"T-02","file":"crates/rally-cli/tests/native_hook.rs","name":"claude_peer_claim_is_high_severity_advisory"},
    {"id":"T-03","file":"crates/rally-cli/tests/native_hook.rs","name":"claude_strict_denies"},
    {"id":"T-04","file":"crates/rally-cli/tests/native_hook.rs","name":"codex_conflict_never_carries_permission_decision"},
    {"id":"T-05","file":"crates/rally-cli/tests/native_hook.rs","name":"pure_reads_return_immediately_without_ledger_work"},
    {"id":"T-06","file":"crates/rally-cli/tests/native_hook.rs","name":"opaque_shell_and_target_cap"},
    {"id":"T-07","file":"crates/rally-cli/tests/native_hook.rs","name":"deadline_miss_is_fail_loud_advisory","mutation_check":"advisory removed → red"},
    {"id":"T-08","file":"crates/rally-cli/tests/native_hook.rs","name":"malformed_no_rally_or_old_binary_fail_open"},
    {"id":"T-09","file":"crates/rally-cli/tests/native_hook.rs","name":"sec001_vectors_refuse_and_fall_back"},
    {"id":"T-10","file":"crates/rally-cli/tests/native_hook.rs","name":"duplicate_event_runs_once"},
    {"id":"T-11","file":"tests/hooks/test_rally_coordination_hook.sh","name":"67 cases, RALLY_NATIVE_HOOK=off, 5x under load"},
    {"id":"T-12","file":"tests/scripts/test_generate_host_surfaces.py","name":"test_native_effect_registry_matches_hook_classifier (extended to hook_runtime.rs)"},
    {"id":"T-13","file":"scripts/bench_hook_latency.py","name":"before/after p50/p95 + attribution"}
  ],
  "adrs": ["A-01","A-02","A-03","A-04"]
}
```

## ADR-01 — `rally hook <phase>` CLI surface (low reversibility)
Alternatives: (a) `rally check before-write --hook-envelope` flag on the existing command; (b) `rally hook <phase>` positional (chosen); (c) `--host` explicit. Tradeoffs: (a) overloads a JSON-envelope command with a host-envelope output and breaks the json_envelope_contract; (b) additive phases, probe-able capabilities; (c) reserved. Rollback: remove `hook` from COMMANDS; shell probe then caches `fallback` and the Node path serves everything — no data migration.

## ADR-02 — host envelope contract owned by Rust (low reversibility)
Alternatives: return rally JSON and let the shell/node render (status quo) vs render in Rust (chosen; the shell renderer remains for fallback + lifecycle). Rollback: `RALLY_NATIVE_HOOK=off`.

## ADR-03 — `WatchdogPosture::HookAdvisory` (medium)
Alternatives: (a) inner deadline only (misses a single blocked syscall — the outer watchdog would print `{"ok":true}` = silent); (b) outer only (cannot say "no claim created" precisely); (c) both, same advisory text (chosen). Rollback: delete the posture arm; T-07a goes red (by design).

## ADR-04 — exec probe cached per (root, binary) (medium)
Alternatives: (a) grep the binary file for a sentinel (no exec, ARP-001-purist, brittle); (b) version compare (needs exec anyway); (c) `rally hook capabilities` once, cached by `-nt` (chosen). Consequence: stub-driven suites must run with `RALLY_NATIVE_HOOK=off` (L6). Rollback: env off.

## Open Questions

1. Should `hook before-write` honour `RALLY_BEFORE_WRITE_FAILCLOSED=1` (deny on deadline miss) like `check before-write` does? Plan says NO (charter; the shell hook never honoured it either since perl killed rally). blocking-test: T-07 (a "yes" changes its assertions). Default taken: `[ASSUMED: no — advisory-only on timeout]`.
2. Should the disabled-hooks case print `{}` (this plan) or empty stdout (shell parity)? Both host-valid. blocking-test: T-08 (empty-or-`{}` accepted). `[ASSUMED: {} ]`.

Other assumptions: `[ASSUMED: RALLY_HOOK_TIMEOUT_MS default 3000 for the native exec]`; `[ASSUMED: dedupe state dir defaults to <root>/.rally/.hook-events rather than the git common dir — native and fallback never interleave within one host session]`; `[ASSUMED: node and git present on the dev host for T-08c/T-09]`; `[ASSUMED: donor function bodies (unseen here — recovered by C1 via git show) match the operator's list; the frozen signatures above take precedence where they differ]`.

## Central design claim and its falsifier

Claim: one Rust process can own the whole before-write transaction under one deadline, cutting p50 ≥5× while preserving every host contract, the fail-loud abort, and the fallback. Falsifier: any of — after.json p50(1 path) > 100 ms with attribution NOT dominated by store I/O; T-02/T-03/T-04 red; T-07 stays green after the mutation (advisory removed); 67-case suite red in fallback mode. Certainty: ⚠️ untested until C5 measures; the shape is ✅ verified against the shell renderer and watchdog code read above.

## Out of Scope (mirror)

Lifecycle phases native; README/host-integrations/version/tag/push; rewriting the Node fallback; RC-037 stdout; budget-scale on native.

---

## Plan-critic revisions (BINDING — these supersede the sections they name)

Reviewed by `plan-critic` at Frontier tier, 2026-08-15. All 12 spot-checked line
citations in the plan above verified accurate. Eight findings; all eight are
adopted. `scope-auditor` (Frontier) returned `scope_gap_found` on one advisory
item, folded into R7.

### R1 — SEC-001 bare-name fallback is a live containment bypass (C3, security boundary)

**Verified by reading `hooks/rally-coordination-hook.sh:649-689.**` The containment
cascade refuses an in-repo `$PATH` candidate (`:653-658`), tries
`~/.local/bin/rally` (`:663`), and then falls back to the bare string
`RALLY_BIN="rally"` (`:670`). The very next check (`:689`) runs
`command -v "$RALLY_BIN"`, which re-resolves that bare name through the SAME
`$PATH` — returning the in-repo binary that was just refused. Every later
`rally_timeout`/exec then runs it. The hole is masked on this dev host only
because `~/.local/bin/rally` exists; a machine without it executes repo-supplied
code. This is pre-existing (not introduced by option A), but C3 owns this block,
so C3 fixes it rather than hoisting it forward.

- Delete the `else RALLY_BIN="rally"` arm. A refused candidate with no
  `~/.local/bin/rally` leaves `RALLY_BIN` EMPTY — both call sites already handle
  "binary missing" (`:689` fail-open + install advisory).
- The native probe (C3 step 4) may only run an ABSOLUTE path that has passed
  `_rally_path_inside_repo`. Bind `command -v` output; never probe a bare name.
- Add the vector to `tests/hooks/test_no_autoprovision.sh` (the only suite that
  pins SEC-001 messages — plan:218's claim that "the 67 cases grep them" is
  wrong; the 67-case suite has zero SEC-001 cases). Keep T-09(ii) as well.

### R2 — T-07 does not falsify the deadline path (C2)

`RALLY_TEST_BLOCK_MS` sleeps in `lib.rs:1362-1367` for EVERY command, so under
T-07(a) the shell's own probe (`hook capabilities`) times out first, caches
verdict `fallback`, and the NODE path emits its own advisory inside 1000 ms.
T-07(a) would pass green without `HookAdvisory` ever running, and the Rust
mutation could not turn it red. T-07(b) fires only the OUTER watchdog, so the
inner `Deadline`/`abort_envelope` path that ADR-03 exists for has no falsifier.

- T-07(a): pre-seed the probe marker to `native` (or take one warm fire and
  assert it) BEFORE exporting the seam, and assert the marker still reads
  `native` afterwards.
- Add T-07(c): a deterministic `#[cfg(debug_assertions)]` seam
  `RALLY_TEST_HOOK_FORCE_DEADLINE=1` (not a timed sleep) that trips the inner
  `Deadline` before the claim append; assert reason `auto-claim skipped (budget)`.
- Run the two mutations SEPARATELY and record both: `abort_envelope` alone must
  turn (c) red; `abort_envelope_from_args` alone must turn (a) and (b) red. A
  single bundled mutation hides which path is actually covered.

### R3 — the perf target has no decision rule at, say, 200 ms (C1, C0, C5)

F-09/Goal state `p50 <= 100 ms` as a pass condition; Risk 1 says record honestly.
The falsifier turns on "attribution NOT dominated by store I/O", but C0 attributes
by counting spawns x unit startup cost — on a one-spawn native path that yields
nothing about store vs classify vs render.

- C1 adds a debug/opt-in seam `RALLY_HOOK_TRACE=1` emitting ONE stderr JSON line
  with per-stage milliseconds: parse, classify, root, open, snapshot, check,
  append, render.
- C0/C5 record that line in the artifacts.
- F-09 becomes an explicit three-branch rule: `<=100 ms` -> pass. `>100 ms` with
  spawn count 1, before/after ratio >= 3x, and open+snapshot+append >= 60% of
  wall -> ship and file the store-open cost as the next lever. Otherwise C1 gets
  one revision pass before C5.

### R4 — Cursor and Gemini envelopes are frozen but unfalsified (C2)

L3/D-01 freeze four host shapes plus the Cursor abort shape; T-02..T-04 cover
only Claude and Codex, so a renderer emitting Claude keys for `cursor`/`gemini`
passes all ten goldens. No new test: extend T-04 to also drive `cursor` and
`gemini` (default and strict) asserting EXACT key sets, and add a `cursor`
sub-case to T-07 asserting `{"permission":"allow","agent_message":...}`.

### R5 — the hooks-disabled path has no test (C2)

Open Question 2 names T-08 as its blocking test, but T-08(a)(b)(c) contain no
disabled case. Add T-08(d): `rally hooks off --scope repo`, then a Write ->
`{}`-or-empty with zero ledger delta. Settled: a MALFORMED hooks config is
treated as ENABLED, matching `hook.sh:440-445`.

### R6 — the shell native branch has no direct falsifier (C4)

Pinning the 67 stub cases to `RALLY_NATIVE_HOOK=off` is correct (those cases
assert an empty CALLS log, which the probe would violate), but it leaves the
exec argv shape, the `-nt` cache invalidation, and the marker-write-failure
fallback unasserted. Add ONE 68th case that overrides the header with
`RALLY_NATIVE_HOOK=on` and a stub answering `hook capabilities` with
`{"data":{"hook":{"phases":["before-write"]}}}` while logging argv; assert the
exact argv (`--tool`, `--repo-root`, `--timeout-ms`, and NO `--fail-open`),
`capabilities` invoked once across two fires, and a re-probe after `touch` on
the stub. Case count becomes 68.

### R7 — two files are owned by no chunk (C2, C5)

Both `plan-critic` and `scope-auditor` land here independently.
`docs/JSON_ENVELOPE.md:91` claims every `rally <cmd> --json` returns the standard
envelope; `hook before-write --json` deliberately will not.

- C2 gains `crates/rally-cli/tests/json_envelope_contract.rs` and adds
  `envelope_hook_capabilities` (schema `agent-rally.command.hook.v1`,
  `data.hook.phases` contains `before-write`), matching the `daemon` precedent at
  that file's lines 337-340.
- C5 gains `docs/JSON_ENVELOPE.md` for the carve-out sentence.

Also from scope-auditor, for the C1 implementer: the help-text test at
`lib.rs:9034` matches `starts_with("rally {command}")`, so the existing
`rally hooks status ...` line ALREADY satisfies `rally hook` — the test will not
catch a missing help line. Add the line unconditionally, and tighten the matcher
to `starts_with("rally {command} ") || == "rally {command}"` (same file, no scope
change). `CliCommand`, `COMMANDS`, and `WatchdogPosture` are otherwise
`internal_only: true`; resolving `HookAdvisory` first is inert for every existing
command (verified against `strip_timeout_flag` and the posture unit tests).

### R8 — T-05's absolute timing bound will flake (C2)

`elapsed < 100 ms` per pure read, on a debug binary, under `cargo test`
parallelism, on a host measured at load-avg 7-9. The `.rally` listing and
`ledger_lines` assertions are the real falsifiers. Make the bound relative
(<= 3x a same-run `rally version` spawn) or drop it.

### R9 — two behaviour changes C3 introduces must be documented (C5)

Because the native branch runs before the envelope is read, pure reads and
config-disabled repos now RESOLVE and EXEC the binary and write a probe marker
before any opt-out is consulted (previously `hook.sh:461-468` / `:529-534` exited
first). `docs/AUTO-COORDINATION-HOOKS.md:128` currently claims the hook is
"silent" when hooks are disabled; C5 must correct that line. Cheap hardening for
T-01 while there: assert stderr is empty and no `*.mutation-abort.seen` marker.

---

## C6 — session/prompt/stop message contract (operator scope amendment, 2026-08-15)

Source: the Addendum at the end of `~/dev/docs/rally-native-hook-build-brief-2026-08-15.md`.
Operator decided this mid-run and it ships with the release. It is a SEPARATE
chunk because it is a different file region and a different contract from the
before-write transaction.

### Scope boundary (the one ambiguity, resolved and labelled)

The amendment names "the SessionStart / UserPromptSubmit / Stop room message"
and locates it as "the node renderer around lines ~1596-1616", i.e. the block
ending at `hook.sh:1601` that emits
`{agent_visible:{present,severity,message}, ledger_data}` for the START phase,
plus the final envelope renderer's preamble decision at `hook.sh:2069`.

Its L1 template list is keyed on `next.action`, whose values include
`before-write conflict`. `[ASSUMED]` — and it is the reading this chunk is built
on — **that value is a ROOM state being reported by a lifecycle message, not the
before-write hook's own envelope.** The amendment says "separate file region
from before-write" in the same sentence, and golden (b) is therefore "a room
message whose next.action is a before-write conflict", not "the PreToolUse
envelope". Consequence if that reading is wrong: C1's Rust `render_before_write`
would also have to emit the 3-line shape, which would supersede the byte-exact
envelope contract C1 was frozen against. That is a one-chunk revision, not a
redesign, but it is the falsifier for this boundary — plan-critic is asked to
attack it specifically.

### Ordering (C3 and C6 share a file — this is deliberate, not an MECE break)

`hooks/rally-coordination-hook.sh` is owned by C3 (before-write exec branch,
lines ~134-144 and the SEC-001 block) and by C6 (lifecycle renderer, ~1560-1601,
and the preamble decision at ~2069). The regions do not overlap, but two chunks
on one file cannot run in parallel. **C6 runs strictly AFTER C3 lands**, and C6
rebases on C3's committed file. C6 additionally owns
`tests/hooks/test_context_sanitization.sh` only if an RC-040 fixture needs its
expectation updated; if none does, C6 must not touch it.

### The contract

L1 Big Idea: one sentence, POV verb + " — " + stakes clause, <=140 chars, from a
fixed template per `next.action` (`respond_to_handoff` / `before-write conflict` /
`release_claim` / `blocked` / `nothing`). NEVER contains peer prose; no guillemets.

L2: starts `Why: `; impact + evidence: the rally id(s) (`fact_...` or `seq A->B`),
who/what blocks, lease minutes. Any peer prose ONLY inside `<<...>>` immediately
followed by `(untrusted)`. **This replaces the 3-sentence UNTRUSTED_PREAMBLE**
(`hook.sh:1346`, applied at `:2069`) — the RC-040 control becomes the
guillemets-plus-tag on the only line that can carry peer text.

L3: starts `Next: `; up to three branches in fixed order act / wait / escalate,
` · `-separated, EXECUTABLE COMMAND FIRST (e.g. `rally say resolve --ref <id>`),
then `wait -> <condition>`, then `operator decision -> <what>`.

States:
- addressed to you -> 3 lines.
- room moved but nothing for you -> 1 line:
  `Nothing needs you — N working elsewhere, no claim on your paths · seq A→B → rally room`
- unchanged `max_seq` since this session's last emit -> `{}` on UserPromptSubmit
  and Stop. SessionStart ALWAYS emits once. Per-session state file
  `.rally/.hook-seen/<session>.last-seq`.
- `RALLY_HOOK_PROMPT_MODE=verbose` restores today's roster; default is `brief`.
  (Today's variable already exists with values `once`/`off` at `hook.sh:1568` —
  C6 must keep `off` working and add `brief` as the default and `verbose`.)

Same text on Claude and Codex; Codex carries it via `additionalContext`.

### Tests (goldens, in `tests/hooks/`)

(a) handoff addressed to you (the live `fact_8c7` CHANGELOG shape)
(b) room message whose `next.action` is a before-write conflict
(c) room moved / nothing for you -> the 1-line form
(d) unchanged seq -> `{}`

Each asserts: <=3 lines; <=420 chars total; L1 <=140 chars with EXACTLY ONE
` — ` and no guillemets; L2 starts `Why: ` and contains an id; every guillemet
span is immediately followed by `(untrusted)`; L3 starts `Next: ` and its first
token is a `rally` command when the action is executable. Run on BOTH
`claude_code` and `codex` and assert identical message text.

`tests/hooks/test_context_sanitization.sh` (the RC-040 GAP fixtures) must still
pass unmodified: peer text must never appear unquoted. Since the preamble is
being removed, re-run that suite FIRST against the new renderer and treat any
failure as a contract defect in C6, not as a fixture to update — the preamble
was one of two controls, and the guillemet tag now carries the whole load.

### files_touched
`hooks/rally-coordination-hook.sh` (lifecycle renderer region + preamble
decision only), `tests/hooks/test_room_message_contract.sh` (new).
modifies_api: false. depends_on: C3. risk_reason: user trust claim (this is the
text an agent reads as instructions; the RC-040 control moves).

### Verification
`bash tests/hooks/test_room_message_contract.sh` green;
`bash tests/hooks/test_context_sanitization.sh` green UNMODIFIED;
`bash tests/hooks/test_rally_coordination_hook.sh` green (its lifecycle-phase
cases will need their expectations updated — that is expected and in scope, and
the case count change must be reported);
one live `claude -p` and one `codex exec` run showing the same 3-line text.
