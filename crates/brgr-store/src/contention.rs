//! Waiting for a lock that another process holds, and deciding when not to.

use std::time::{Duration, Instant};

use super::StoreError;

/// How long `SQLite` itself waits for a lock before reporting busy. This is the
/// dominant term in any contended store operation: a retry loop on top of it
/// multiplies this wait, it does not replace it.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Total time [`retry_busy`] may spend re-attempting an operation that failed
/// only because another process held a lock. Bounded so a contended command
/// fails with a reason instead of hanging.
///
/// Only paths that hold no cross-process lock of their own may use this. Waiting
/// here while holding one converts a single failure into two: measured on
/// 2026-09-29, a retrying task admission held the repository admission lock for
/// 17.5s and still failed, while a second admission gave up after 10.2s with
/// `another admission is in progress` — a misleading error for a store problem.
/// Without the retry the holder failed in about 5s and released the lock.
///
/// The wait is a blocking sleep, and the retried paths are reached from async code
/// in `brgr-core`, so a contended write occupies a runtime worker. That is not
/// this loop's doing: `busy_timeout` blocks the thread inside `SQLite` for up to
/// five seconds per attempt whether or not a retry follows, so every store call
/// from async code already holds a worker. Moving store work off the runtime is a
/// change to how the store is called, not to this budget, and is tracked in the
/// readiness checklist rather than papered over here. The budget bounds how much
/// this loop can add to it.
pub(crate) const BUSY_RETRY_BUDGET: Duration = Duration::from_secs(10);
pub(crate) const BUSY_RETRY_BACKOFF: Duration = Duration::from_millis(2);
pub(crate) const BUSY_RETRY_BACKOFF_CAP: Duration = Duration::from_millis(250);

/// Retries an operation that failed only because another process held a lock.
///
/// Store mutations are idempotent and a failed transaction has already rolled
/// back, so replaying one cannot apply it twice. Anything that is not lock
/// contention is returned on its first occurrence.
///
/// The whole retry loop is bounded by one budget rather than an attempt count,
/// because each attempt may itself wait out `busy_timeout` before failing; a
/// per-attempt count would let the worst case grow with that timeout.
pub(crate) fn retry_busy<T>(
    operation: impl FnMut() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    retry_busy_within(BUSY_RETRY_BUDGET, operation)
}

/// [`retry_busy`] with an explicit budget, so a test can exercise the loop
/// without waiting out the production one.
pub(crate) fn retry_busy_within<T>(
    budget: Duration,
    mut operation: impl FnMut() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let deadline = Instant::now() + budget;
    let mut delay = BUSY_RETRY_BACKOFF;
    loop {
        match operation() {
            Err(error) if is_lock_contention(&error) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                std::thread::sleep(delay.min(deadline - now));
                delay = delay.saturating_mul(2).min(BUSY_RETRY_BACKOFF_CAP);
            }
            outcome => return outcome,
        }
    }
}

/// Reports whether an error is transient lock contention rather than a rejected
/// write. `SQLITE_BUSY_SNAPSHOT` reports the same primary code.
pub(crate) fn is_lock_contention(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Database(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_busy_replays_contention_and_passes_other_errors_through() {
        let mut attempts = 0;
        // Contention that clears is replayed until it succeeds.
        let outcome = retry_busy(|| {
            attempts += 1;
            if attempts < 3 {
                return Err(StoreError::Database(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(517),
                    None,
                )));
            }
            Ok(attempts)
        })
        .unwrap();
        assert_eq!(outcome, 3);

        // A rejected write is returned on its first occurrence, never replayed.
        let mut calls = 0;
        let error = retry_busy(|| {
            calls += 1;
            Err::<(), _>(StoreError::InvalidTaskLimit)
        })
        .unwrap_err();
        assert!(matches!(error, StoreError::InvalidTaskLimit));
        assert_eq!(calls, 1);
    }
    #[test]
    fn retry_busy_gives_up_only_after_its_budget() {
        let budget = Duration::from_millis(80);
        let start = Instant::now();
        let mut calls = 0;
        let error = retry_busy_within(budget, || {
            calls += 1;
            Err::<(), _>(StoreError::Database(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(5),
                None,
            )))
        })
        .unwrap_err();
        assert!(is_lock_contention(&error));
        assert!(calls > 1, "contention was not retried at all");
        let elapsed = start.elapsed();
        assert!(elapsed >= budget, "gave up before its budget: {elapsed:?}");
        assert!(
            elapsed < budget * 4,
            "retry overran its budget: {elapsed:?}"
        );
    }
    #[test]
    fn only_lock_contention_is_retried() {
        let busy = StoreError::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5),
            None,
        ));
        assert!(is_lock_contention(&busy));
        let snapshot = StoreError::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(517),
            None,
        ));
        assert!(is_lock_contention(&snapshot));
        let full = StoreError::Database(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(13),
            None,
        ));
        assert!(!is_lock_contention(&full));
        assert!(!is_lock_contention(&StoreError::InvalidTaskLimit));
    }
}
