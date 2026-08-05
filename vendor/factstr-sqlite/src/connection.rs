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

/// Pool checkout is a separate blocking boundary before SQLite can execute a
/// statement. Keep it a strict fraction of the caller's SQLite busy budget so
/// the two waits compose inside the same external deadline instead of each
/// consuming the full blocking allowance.
pub const ACQUIRE_TIMEOUT_DIVISOR: u32 = 4;

pub fn acquire_timeout_for(busy_timeout: Duration) -> Duration {
    // sqlx requires a positive acquire timeout. One nanosecond is effectively
    // non-blocking for the Duration::ZERO boundary.
    (busy_timeout / ACQUIRE_TIMEOUT_DIVISOR).max(Duration::from_nanos(1))
}

pub(crate) async fn open_pool(
    database_path: &Path,
    busy_timeout: Duration,
    acquire_timeout: Option<Duration>,
) -> Result<SqlitePool, sqlx::Error> {
    let connect_options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(busy_timeout)
        .foreign_keys(true);

    let pool_options = SqlitePoolOptions::new().max_connections(4);
    let pool_options = match acquire_timeout {
        Some(timeout) => pool_options.acquire_timeout(timeout),
        // Preserve sqlx's upstream default for `SqliteStore::open`; only the
        // explicit deadline-aware constructor couples this boundary.
        None => pool_options,
    };
    pool_options.connect_with(connect_options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_timeout_is_a_strict_fraction_of_sqlite_busy_wait() {
        assert_eq!(
            acquire_timeout_for(Duration::from_millis(375)),
            Duration::from_micros(93_750)
        );
        assert_eq!(
            acquire_timeout_for(DEFAULT_BUSY_TIMEOUT),
            Duration::from_millis(1250)
        );
        assert_eq!(acquire_timeout_for(Duration::ZERO), Duration::from_nanos(1));
    }
}
