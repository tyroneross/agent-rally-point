<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Cargo Quality Gate Recommendations - 2026-07-03

Status as of `f1f0769` (`ci(fmt): pin Rust 1.95.0 + add cargo fmt --check gate`).

## Recommendation

Treat the Rally reliability lane as shipped, but do not treat the Cargo/build hygiene lane as complete yet. The immediate fmt gap is closed by Claude's `9bbead3` and `f1f0769`, but clippy, dependency audit, cargo-deny policy, and package verification remain open.

Recommended gate ladder:

1. Keep `cargo fmt --all --check` in CI under the pinned Rust toolchain.
2. Fix current clippy findings, then add `cargo clippy --workspace --all-targets -- -D warnings` to CI.
3. Resolve dependency-audit findings before adding a hard `cargo audit` gate.
4. Add a project-owned `deny.toml`, then add `cargo deny check`.
5. Decide package intent for `rally-cli`: binary-only release with `publish = false`, or crates.io-ready package with metadata and a versioned internal dependency on `rally-protocol`.

## Current Evidence

Verified locally on 2026-07-03:

| Check | Current result | Interpretation |
|---|---:|---|
| `cargo fmt --check --all` | Pass | Closed by `9bbead3` plus CI gate in `f1f0769`. |
| `git diff --check` | Pass | No whitespace errors in the current checkout. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Fail | 13 `rally-cli` clippy diagnostics remain. |
| `cargo audit` | Fail | `RUSTSEC-2023-0071` via `rsa 0.9.10`; warning on `anyhow 1.0.102`. |
| `cargo deny check` | Not gateable yet | No `deny.toml`; default config rejects common licenses and creates unusable noise. |
| `cargo package -p rally-cli --allow-dirty` | Fail | `rally-protocol` path dependency has no version requirement; manifest metadata also incomplete. |

## What Claude Already Closed

Claude closed the formatter drift RCA:

- `9bbead3` - ran rustfmt over the current `rally-cli` drift under Rust `1.95.0` / rustfmt `1.9.0`.
- `f1f0769` - pinned CI to `dtolnay/rust-toolchain@1.95.0`, added `rustfmt` and `clippy` components, and added `cargo fmt --all --check`.

This addresses the earlier "cargo fmt fails on unrelated drift" escape path: CI now detects formatter drift before merge.

## Remaining Findings

### P1 - Clippy Gate Is Not Ready

Current `cargo clippy --workspace --all-targets -- -D warnings` fails in `rally-cli`.

Failure categories:

- `manual_range_contains` in `crates/rally-cli/src/backends.rs`.
- `question_mark` in `crates/rally-cli/src/backends.rs`.
- `double_ended_iterator_last` and `too_many_arguments` in `crates/rally-cli/src/backlog.rs`.
- `iter_overeager_cloned` in `crates/rally-cli/src/next.rs`.
- test-only `bool_assert_comparison` in `crates/rally-cli/src/backends.rs`.
- `doc_lazy_continuation` in `crates/rally-cli/src/reaper.rs`.
- `useless_format` in `crates/rally-cli/src/store.rs`.

Recommendation: fix the mechanical items directly, then decide whether backlog argument-count warnings should be refactored into a request/options struct or explicitly allowed with rationale. Do not add the clippy CI gate until the tree is green.

### P1 - Dependency Audit Is Red

`cargo audit` reports:

- `RUSTSEC-2023-0071`, `rsa 0.9.10`, through `factstr-sqlite -> sqlx -> sqlx-mysql -> rsa`. The advisory reports no fixed upgrade.
- `RUSTSEC-2026-0190`, `anyhow 1.0.102`, warning class. `cargo update -p anyhow --precise 1.0.103 --dry-run` shows this one is directly updatable.

Recommendation:

1. Update `anyhow` to `1.0.103` or newer through Cargo, then rerun tests and audit.
2. Investigate why the SQLite path brings `sqlx-mysql` into the resolved graph. Prefer feature pruning or upstream feature change over an audit ignore.
3. If `rsa` is truly unreachable in Rally's shipped binary, document the reachability evidence and add a narrow `audit.toml` ignore with an owner and review date.

### P2 - cargo-deny Needs A Policy Before It Can Be A Gate

`cargo deny check` currently finds no config and falls back to a default policy that rejects common permissive licenses such as `MIT OR Apache-2.0`. That output is too noisy to gate.

Recommendation: add `deny.toml` with explicit allowed licenses, advisory policy, duplicate-dependency policy, and allowed sources. Only then add `cargo deny check` to CI.

### P2 - Package Intent Is Ambiguous

`cargo package -p rally-cli --allow-dirty` fails because `rally-cli` depends on `rally-protocol` as:

```toml
rally-protocol = { path = "../rally-protocol" }
```

Cargo package verification requires a version requirement because path dependencies are removed for registry packaging.

Recommendation:

- If `rally-cli` is only shipped as GitHub release binaries, set `publish = false` and keep package checks focused on `rally-protocol`.
- If `rally-cli` should be crates.io installable, add package metadata and make the dependency versioned, for example `rally-protocol = { version = "0.1.0", path = "../rally-protocol" }`, then verify publish order.

### P2 - Manifest Metadata Is Incomplete

`cargo package --list` and `cargo package` warn that `rally-cli` has no description, documentation, homepage, or repository. `rally-protocol` packages successfully but warns that documentation/homepage/repository are missing.

Recommendation:

- Add `repository` at minimum for publishable packages.
- Add `description` to `rally-cli` if it remains packageable.
- Use `publish = false` for internal-only crates such as cockpit packages if they are not meant for registry publication.

## CI Target State

Short-term CI target:

```yaml
- run: cargo fmt --all --check
- run: cargo clippy --workspace --all-targets -- -D warnings
- run: cargo test
- run: cargo build --release -p rally-cli
- run: cargo audit
- run: cargo deny check
- run: cargo package -p rally-protocol --allow-dirty
```

Add `cargo package -p rally-cli` only after the publish model is decided.

## Source Basis

- Cargo manifest metadata and `publish = false`: <https://doc.rust-lang.org/cargo/reference/manifest.html>
- Cargo lockfile guidance: <https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html>
- Cargo workspaces and shared metadata: <https://doc.rust-lang.org/cargo/reference/workspaces.html>
- Dependency publishing constraints: <https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html>
- Package verification behavior: <https://doc.rust-lang.org/cargo/commands/cargo-package.html>
- RustSec cargo-audit behavior: <https://github.com/rustsec/rustsec/blob/main/cargo-audit/README.md>
- cargo-deny dependency-graph policy checks: <https://embarkstudios.github.io/cargo-deny/>
- cargo-vet third-party audit model: <https://mozilla.github.io/cargo-vet/>
- Rust API Guidelines metadata and public API checklist: <https://rust-lang.github.io/api-guidelines/checklist.html>
