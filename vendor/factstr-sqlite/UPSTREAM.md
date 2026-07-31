# Vendored factstr-sqlite

- Upstream: https://github.com/ricofritzsche/factstr
- Package: `factstr-sqlite` 0.5.2
- Upstream commit: `8336fc780aa6d197d9ab2a2acf610b84810cc5d5`
- License: MIT OR Apache-2.0

Rally vendors this package because upstream 0.5.2 drops its sqlx pool
asynchronously. In multi-process direct mode, the delayed final SQLite close
can checkpoint and unlink a live WAL after Rally releases its mutation lock.

The local delta closes the pool synchronously in `SqliteStore::drop`. Remove
this vendor override after an upstream release includes an equivalent close
guarantee and Rally's adversarial parallel-launch test passes against it.
