<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Changelog

All notable changes to Agent Rally Point are documented here.

## Unreleased

### Changed

- Cut the product architecture over to Rust. The user-facing command is `rally`.
- Removed the legacy Python runtime package, Python packaging metadata, and
  legacy discovery/migration documentation.
- Kept the durable product contract centered on `changes.jsonl`, portable
  events, signed trust, sync packets, and stable JSON command envelopes.

### Verification

- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `git diff --check`
