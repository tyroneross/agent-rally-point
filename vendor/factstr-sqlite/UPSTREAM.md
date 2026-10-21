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

## Delta 2 — caller-supplied SQLite busy timeout (2026-08-05)

Upstream hardcodes `busy_timeout(5s)` in `connection.rs`. `busy_timeout` is how
long SQLite blocks INSIDE one call waiting for a lock before returning
`SQLITE_BUSY`, so it is a deadline — and 5s is longer than the entire 3000ms
wall-clock watchdog every `rally` command runs under.

The consequence was not a slow command but an unreachable one: SQLite swallowed
the lock error for 5s while rally's watchdog killed the process at 3s, so
rally's own lock-retry loops never executed a single iteration. Measured against
a genuine `BEGIN EXCLUSIVE` holder, `rally say claim` exited 4 at 3.036s with
`watchdog-timeout-uncommitted-mutation`, every time, regardless of how rally
configured its retries.

The delta adds `SqliteStore::open_with_busy_timeout` and leaves
`SqliteStore::open` delegating to it with `DEFAULT_BUSY_TIMEOUT` (upstream's 5s),
so any other consumer keeps the upstream 5s database setting. Rally passes an
eighth of its remaining watchdog budget, and sqlx pool acquisition is capped at
one quarter of that duration. `SqliteStore::open` keeps sqlx's upstream pool
acquisition default; the coupling applies only to the explicit deadline-aware
constructor.

Upstreamable as-is: it is additive, preserves the existing default, and a
library cannot know its caller's deadline. Remove this delta if upstream adopts
an equivalent constructor.

## Delta: one runtime per store, drained before pool close (rally, 2026-10-20)

`SqliteStore` built a fresh `tokio` current_thread runtime for every operation
(bootstrap, each `run_async`, the delivery thread, and close). sqlx-core 0.8.6
returns a dropped `PoolConnection` by spawning `return_to_pool` onto the
*current* runtime. When the per-operation runtime was dropped, that task was
cancelled and the connection was dropped without a graceful close. The pool
then reported size 0, `pool.close()` returned at once, and the sqlx-sqlite
worker thread ran `sqlite3_close` (WAL checkpoint and `-wal` removal)
afterwards, so `facts.db` was still being written after the caller believed
the store was closed.

The delta holds one `Arc<Runtime>` per store and drives bootstrap, every
operation, delivery, and close on it, so the return tasks outlive the
operations that queued them. Close first drives that runtime until every live
connection is idle (`num_idle() == size()`), then calls `pool.close()`. The
wait is required: `Pool::close` in sqlx 0.8.6 does not wait for a pending
return task (closing an idle connection releases a permit it never held), and
a connection closing concurrently with another leaves neither as the last
handle, so SQLite keeps the `-wal` file. Closing idle connections one at a
time makes the last `sqlite3_close` delete it.

Public API is unchanged. Upstreamable as-is; remove this delta if upstream
adopts a shared runtime or a pool close that waits for return tasks.
