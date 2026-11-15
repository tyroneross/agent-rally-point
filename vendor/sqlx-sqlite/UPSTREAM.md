<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Vendored SQLx SQLite driver

- Source package [sqlx-sqlite 0.8.6](https://crates.io/crates/sqlx-sqlite/0.8.6)
- Registry checksum `c2d12fe70b2c1b4401038055f90f151b78208de1f9f89a7dbfd41587a10c3eea`
- License MIT OR Apache-2.0

This copy preserves upstream Rust behavior. It normalizes three rustdoc line
breaks and changes one manifest constraint. SQLx 0.8.6 requires `libsqlite3-sys` 0.30.1, and Cargo allows one
SQLite native library per graph. The driver accepts `libsqlite3-sys` 0.39.1
through 0.39.x so it can share bundled SQLite 3.54.1 with `rusqlite` 0.41.
That SQLite release fixes [RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html).

Remove this override when an upstream SQLx release supports the patched native
wrapper and Rally’s Rust minimum. Re-run the direct-store, daemon, and WAL
handover tests before dropping it.
