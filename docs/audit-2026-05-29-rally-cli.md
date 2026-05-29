<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# rally-cli audit — 2026-05-29 (scaled multi-agent dogfood)

> Produced by the dynamic-workflows scale dogfood: workflow `wnll6qruh`, **10 Sonnet read-only `Explore` agents** across two fan-out scales. **Scale A** (3 agents: `lib.rs`,`backends.rs`,`next.rs`) → 16 findings; **Scale B** (7 agents, one per remaining `src/*.rs`) → 18 findings. **34 total · 8 high · 14 medium · 12 low**, 0 agent failures, ~3.3 min, 266K subagent tokens. Findings are *self-reported by the audit agents* — confirm before fixing.
>
> **Routing:** all findings are in `crates/rally-cli/**` (the Codex/managed-session lane). The lead flags; lane owners fix. Backlog items B4–B7 below track these.

## HIGH (8)

### Boundary-gate integrity — `check.rs` (B4, security-relevant)
The `rally check before-write` gate the whole coordination model relies on can be bypassed three ways:
- **`check.rs:70`** — `allow = !stop || !strict` ⇒ `allow=true` in warn mode even when stop-severity findings (claimed-path, active-blocker) exist. A caller gating on `allow` proceeds through a blocked path. **Fix:** `let allow = !stop;` (use exit code for strict/warn, not `allow`).
- **`check.rs:104-115`** — missing `--path` early-returns, skipping *all* claim/decision/blocker checks. Gate fully bypassed when `--path` omitted (CLI allows it). **Fix:** don't early-return; still evaluate global/empty-scope facts.
- **`check.rs:120-122`** *(medium, same cluster)* — `--tool` omitted defaults to `"unknown"`; two agents both omitting it present as `"unknown"`, so cross-agent conflict detection is suppressed. **Fix:** require `--tool` for before-write (or emit a stop finding when `unknown`).

### NUL-byte panics — shell quoting (B5)
- **`lib.rs:950`** — `shell_quote` `.expect()`s `shlex::try_quote`; any path/`--text` with a NUL byte panics the process. **Fix:** return `Result`, map to `RallyError::Usage`.
- **`backends.rs:497`** — `shell_words` `.expect()`s `shlex::try_join`; crafted session name/target/agent arg with NUL panics. **Fix:** return `Result`, propagate via `?` in `tmux_start_command`/`cmux_start_command`.
- *(next.rs callers at 436–486 inherit this — low, same root.)*

### Data-integrity (B6)
- **`lib.rs:337-343`** — error-recovery cleanup `let _ = append_fact(... stopped ...)` discards its `Result`; on write failure the session stays permanently "active", blocking same-identity restart. **Fix:** log/propagate.
- **`discovery.rs:95`** — `refresh_room_index` reads via `unwrap_or_default()`; a corrupt index is treated as empty then overwritten → **all recorded rooms destroyed**. **Fix:** `?`-propagate or warn+return.
- **`store.rs:322,342,359,375`** — unchecked `u64 as i64` on sequence numbers; values > `i64::MAX` wrap to negative seq silently. **Fix:** `i64::try_from(...)?` at all four sites (or make `Fact::seq` `u64`).

## MEDIUM (14, → B7)
`lib.rs:115` set_cursor discard (attention re-delivery) · `lib.rs:1161-1169` envelope swallows serialization error → empty `{}` · `lib.rs:1176` repo_root falls back to worktree dir (wrong scope root) · `backends.rs:388-408` cmux target parse falls back to arbitrary first line · `backends.rs:341-351` herdr focused-tab not excluded · `next.rs:561` build_attention uses `scope.contains` vs `path_matches_scope` (inconsistent w/ build_entry) · `next.rs:213` score/confidence disagree (score unclamped) · `cli.rs:514` `bounded_i64_arg` silently clamps out-of-range instead of erroring · `check.rs:120-122` unknown-tool conflict suppression (see B4) · `discovery.rs:450-455` rename error-context src/dst swapped · `discovery.rs:290-302` TOCTOU in open_indexed_room · `discovery.rs:221` known_room_for_current hardcodes facts_db path · `store.rs:484-494` set_cursor error-context src/dst swapped · `store.rs:498-514` TOCTOU in read_cursors (NotFound not handled).

## LOW (12)
canonicalize fallbacks swallow IO errors (`lib.rs:1185-1202`) · `wait_for_resolution` full rescan + fail-fast on transient IO (`lib.rs:905-923`) · `from_utf8_lossy` hides non-UTF8 backend output (`backends.rs:486-491`) · format-string `tool` shadow fragility (`next.rs:436-489`) · waiting_on handoff/blocker kind filter (`next.rs:337`) · dead `parse_failure_message` arm (`cli.rs:192`) · aliased flags silently drop second (`cli.rs:526`) · owned-blocker warn-not-stop in before-complete (`check.rs:186-198`) · `idx as i64` wrap (`discovery.rs:379`) · infallible-serialize dead fallback (`output.rs:31`) · stale room.db remove error suppressed (`store.rs:286`) · `Fact::seq==0` overwrite of legit seq 0 (`store.rs:124-126`).

## Scale observation
Big-file fan-out (Scale A) had higher finding density (16/3 ≈ 5.3 per agent) than per-file fan-out (Scale B, 18/7 ≈ 2.6) — the large modules (`lib.rs` 35KB, `next.rs` 18KB, `backends.rs` 17KB) carry most of the risk surface. Both scales ran cleanly with no agent failures; per-file fan-out gives finer attribution, big-file fan-out gives higher yield per agent.
