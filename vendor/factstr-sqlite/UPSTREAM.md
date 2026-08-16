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
