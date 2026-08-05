use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

/// Upstream's SQLite busy timeout, kept as the default so any consumer that
/// calls `SqliteStore::open` sees byte-identical behaviour to 0.5.2.
///
/// Callers that run under a shorter wall-clock deadline than this MUST pass
/// their own via `SqliteStore::open_with_busy_timeout` — see the rally delta in
/// `UPSTREAM.md`. A busy timeout longer than the caller's deadline means SQLite
/// blocks INSIDE a single call past the point where the caller is killed, so
/// the caller's own retry and timeout logic never runs at all.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn open_pool(
    database_path: &Path,
    busy_timeout: Duration,
) -> Result<SqlitePool, sqlx::Error> {
    let connect_options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(busy_timeout)
        .foreign_keys(true);

    // Pool checkout is another blocking boundary. Couple it to the same
    // caller-supplied budget as SQLite's busy handler so a saturated pool
    // cannot wait longer than the database lock policy it serves. sqlx
    // requires a positive acquire timeout; one nanosecond is effectively
    // non-blocking when the caller supplies Duration::ZERO.
    let acquire_timeout = busy_timeout.max(Duration::from_nanos(1));
    SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(acquire_timeout)
        .connect_with(connect_options)
        .await
}
