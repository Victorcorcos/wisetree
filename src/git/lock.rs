//! Transparent recovery from git lock contention (`index.lock`, ref locks,
//! `config.lock`, …).
//!
//! git guards mutations with a create-exclusive `*.lock` file that it unlinks
//! when done. A second git process that finds the lock already present fails
//! with `Unable to create '<path>.lock': File exists.` Two things produce that
//! for wisetree: a *live* concurrent git process (wisetree's own dashboard
//! status poll, an editor's git integration) that holds the lock for only a
//! few milliseconds, and an *orphaned* lock left behind when a git process was
//! killed or crashed mid-operation (the case a user hit during a Bugkill
//! `git revert`).
//!
//! [`retry_on_git_lock`] recovers from both without human intervention: it
//! backs off and retries so a live holder's brief lock simply clears, and — if
//! the same lock sits untouched past [`GIT_LOCK_STALE_AFTER`] — reclaims the
//! orphan before retrying. A lock a running process is actively cycling always
//! has a fresh mtime, so it is only ever waited on, never deleted. Every git
//! call in wisetree flows through one of the two base wrappers
//! (`git::exec::execute_git_command` and the dashboard service's
//! `run_command`), and both route their spawn through here, so the whole
//! codebase inherits the recovery from a single place.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Recovery budget for a git op blocked on a `*.lock`. `RETRIES * BACKOFF`
/// (~3s) comfortably exceeds [`GIT_LOCK_STALE_AFTER`], so a genuinely orphaned
/// lock is always reclaimed with retries to spare, while a lock a live process
/// is actively cycling is only ever waited on.
pub const GIT_LOCK_RETRIES: usize = 12;
pub const GIT_LOCK_BACKOFF: Duration = Duration::from_millis(250);
pub const GIT_LOCK_STALE_AFTER: Duration = Duration::from_secs(2);

/// Extract the `*.lock` path from a git "unable to create/lock" stderr, or
/// `None` when the failure is anything else.
///
/// git single-quotes the lock file it could not create — either as the whole
/// message (`Unable to create '<dir>/index.lock': File exists.`) or nested
/// inside a ref-lock failure (`cannot lock ref 'refs/heads/x': Unable to
/// create '<dir>/x.lock': File exists.`). The quoted `.lock` path is stable
/// across locales; the surrounding prose is not, so we key off the path and a
/// small set of lock-acquisition markers rather than parse the prose. The
/// markers also stop an unrelated error that merely *names* a `*.lock` file
/// (e.g. a pathspec) from being mistaken for lock contention.
pub fn git_lock_path(stderr: &str) -> Option<PathBuf> {
    const MARKERS: [&str; 4] = [
        "Unable to create",
        "cannot lock",
        "File exists",
        "Another git process",
    ];
    if !MARKERS.iter().any(|marker| stderr.contains(marker)) {
        return None;
    }
    // Take the first single-quoted segment that names a lock file; for a
    // ref-lock failure that is the *second* quoted token (after the ref name).
    let mut rest = stderr;
    while let Some(open) = rest.find('\'') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('\'') else {
            break;
        };
        let candidate = &after[..close];
        if candidate.ends_with(".lock") {
            return Some(PathBuf::from(candidate));
        }
        rest = &after[close + 1..];
    }
    None
}

/// Best-effort removal of an orphaned git lock. A live git operation creates
/// and unlinks its lock within milliseconds, so a lock untouched for at least
/// `stale_after` was left behind by a crashed or killed process and is safe to
/// reclaim; a fresher lock is left alone so we never delete one a running
/// process still owns. Any error (already gone, unreadable mtime, permissions)
/// is ignored — the caller simply retries.
pub async fn remove_stale_lock(lock_path: &Path, stale_after: Duration) {
    let Ok(meta) = tokio::fs::metadata(lock_path).await else {
        return;
    };
    // Treat an unreadable/future mtime as stale: better to reclaim than to
    // wedge the pipeline on a lock we can't reason about.
    let stale = meta
        .modified()
        .map(|modified| {
            modified
                .elapsed()
                .map(|age| age >= stale_after)
                .unwrap_or(true)
        })
        .unwrap_or(true);
    if stale {
        let _ = tokio::fs::remove_file(lock_path).await;
    }
}

/// Run `run`, and if its outcome is a git-lock failure (as reported by
/// `lock_of`), back off and retry — reclaiming a stale lock — until it
/// succeeds or the retry budget is spent. `run` is re-invoked on each retry,
/// so it must be safe to repeat: a git op that failed to *acquire* its lock
/// never changed anything, so re-running is a no-op-then-retry. Any outcome
/// `lock_of` does not classify as lock contention is returned immediately on
/// the first try, so the common (no-contention) path pays nothing.
///
/// Generic over the outcome `T` so both base wrappers can share it: the
/// canonical wrapper yields a `GitCommandResult`, the dashboard service a
/// `Result<String, String>`; each supplies the matching `lock_of`.
pub async fn retry_on_git_lock<T, F, Fut>(mut run: F, lock_of: impl Fn(&T) -> Option<PathBuf>) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = T>,
{
    for attempt in 0..=GIT_LOCK_RETRIES {
        let outcome = run().await;
        let Some(lock_path) = lock_of(&outcome) else {
            return outcome;
        };
        if attempt == GIT_LOCK_RETRIES {
            return outcome;
        }
        tokio::time::sleep(GIT_LOCK_BACKOFF).await;
        remove_stale_lock(&lock_path, GIT_LOCK_STALE_AFTER).await;
    }
    unreachable!("the final attempt returns instead of looping")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_lock_path_extracts_index_lock() {
        let stderr = "error: Unable to create \
             '/repo/.git/worktrees/wt/index.lock': File exists.\n\n\
             Another git process seems to be running in this repository";
        assert_eq!(
            git_lock_path(stderr),
            Some(PathBuf::from("/repo/.git/worktrees/wt/index.lock"))
        );
    }

    #[test]
    fn git_lock_path_extracts_nested_ref_lock() {
        // A ref-lock failure quotes the ref name first, then the .lock path —
        // detection must reach past the first quoted token.
        let stderr = "error: cannot lock ref 'refs/heads/main': Unable to \
             create '/repo/.git/refs/heads/main.lock': File exists.";
        assert_eq!(
            git_lock_path(stderr),
            Some(PathBuf::from("/repo/.git/refs/heads/main.lock"))
        );
    }

    #[test]
    fn git_lock_path_ignores_non_lock_failures() {
        // A pathspec that merely names a `.lock` file is not lock contention:
        // no lock-acquisition marker is present.
        assert_eq!(
            git_lock_path("error: pathspec 'foo.lock' did not match any files"),
            None
        );
        // A lock-acquisition marker with no quoted `.lock` path is also not it.
        assert_eq!(git_lock_path("fatal: Unable to create the thing"), None);
        assert_eq!(git_lock_path("merge conflict in file"), None);
    }

    #[tokio::test]
    async fn remove_stale_lock_reclaims_orphan_but_keeps_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Younger than the window: a live process may still own it — kept.
        let fresh = tmp.path().join("fresh.lock");
        std::fs::write(&fresh, b"").unwrap();
        remove_stale_lock(&fresh, Duration::from_secs(3600)).await;
        assert!(fresh.exists(), "a freshly-created lock must be preserved");

        // Older than the window (zero threshold => any existing lock) — removed.
        let stale = tmp.path().join("stale.lock");
        std::fs::write(&stale, b"").unwrap();
        remove_stale_lock(&stale, Duration::ZERO).await;
        assert!(!stale.exists(), "an orphaned lock must be removed");

        // Missing lock is a no-op, never an error.
        remove_stale_lock(&tmp.path().join("gone.lock"), Duration::ZERO).await;
    }

    #[tokio::test]
    async fn retry_on_git_lock_retries_until_success() {
        use std::cell::Cell;

        // Fails with a lock error twice, then succeeds — the driver must keep
        // retrying and return the eventual success.
        let calls = Cell::new(0);
        let outcome: Result<&str, String> = retry_on_git_lock(
            || {
                let n = calls.get();
                calls.set(n + 1);
                async move {
                    if n < 2 {
                        Err("Unable to create '/r/.git/index.lock': File exists.".to_string())
                    } else {
                        Ok("done")
                    }
                }
            },
            |res: &Result<&str, String>| res.as_ref().err().and_then(|e| git_lock_path(e)),
        )
        .await;
        assert_eq!(outcome, Ok("done"));
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn retry_on_git_lock_passes_through_non_lock_error() {
        use std::cell::Cell;

        // A non-lock failure returns immediately, without a single retry.
        let calls = Cell::new(0);
        let outcome: Result<&str, String> = retry_on_git_lock(
            || {
                calls.set(calls.get() + 1);
                async move { Err("fatal: not a git repository".to_string()) }
            },
            |res: &Result<&str, String>| res.as_ref().err().and_then(|e| git_lock_path(e)),
        )
        .await;
        assert!(outcome.is_err());
        assert_eq!(calls.get(), 1, "non-lock errors must not be retried");
    }
}
