//! One writer at a time.
//!
//! The shell hook starts `agentbridge sync` in the background for every new
//! terminal, `auto watch` runs its own pass every few seconds, and the operator
//! can run `sync`, `pull` or `unsync` by hand. All of them read the manifest,
//! change it, and write it back, so two at once lose each other's updates
//! (a created copy missing from the manifest can never be pulled or unsynced).
//!
//! The lock is an OS advisory lock on a file in the data dir. It is released
//! when the process exits for any reason, so a crashed run cannot leave a stale
//! lock behind, and it never needs cleaning up by hand.
//!
//! Read-only commands (`ls`, `info`, `status`, `--dry-run`) do not take it:
//! the manifest is replaced atomically, so they always see a whole file.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long an interactive command waits for another run to finish.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "another agentbridge run{holder} is still working; gave up after {waited:?} \
         (try again in a moment)"
    )]
    Busy { holder: String, waited: Duration },
    #[error("cannot open lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Held for as long as this value lives.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

fn lock_path() -> PathBuf {
    crate::sync::data_dir().join("agentbridge.lock")
}

/// Take the lock, waiting up to `wait` for another run to release it.
pub fn acquire(wait: Duration) -> Result<RunLock, LockError> {
    let path = lock_path();
    let io = |source| LockError::Io {
        path: path.clone(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(io)?;

    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                if start.elapsed() >= wait {
                    return Err(LockError::Busy {
                        holder: holder_hint(&path),
                        waited: wait,
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
    }

    // For the next waiter's error message only; the lock itself is the OS lock.
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    Ok(RunLock { _file: file })
}

/// Take the lock with the default wait.
pub fn acquire_default() -> Result<RunLock, LockError> {
    acquire(DEFAULT_WAIT)
}

fn holder_hint(path: &PathBuf) -> String {
    match std::fs::read_to_string(path) {
        Ok(pid) if !pid.trim().is_empty() => format!(" (pid {})", pid.trim()),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::sync::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTBRIDGE_DATA_DIR", tmp.path()) };
        (tmp, guard)
    }

    #[test]
    fn test_second_acquire_waits_then_reports_busy() {
        let (_tmp, _g) = sandbox();
        let first = acquire(Duration::from_secs(1)).unwrap();

        let start = Instant::now();
        let second = acquire(Duration::from_millis(300));
        let waited = start.elapsed();

        match second {
            Err(LockError::Busy { holder, .. }) => {
                assert!(
                    holder.contains(&std::process::id().to_string()),
                    "message should name the holder, got {holder:?}"
                );
            }
            other => panic!("expected Busy, got {other:?}"),
        }
        assert!(
            waited >= Duration::from_millis(250),
            "returned too early: {waited:?}"
        );
        drop(first);
    }

    #[test]
    fn test_lock_is_released_on_drop() {
        let (_tmp, _g) = sandbox();
        drop(acquire(Duration::from_secs(1)).unwrap());
        acquire(Duration::from_millis(200)).expect("a dropped lock must be free again");
    }

    #[test]
    fn test_waiter_gets_the_lock_when_holder_finishes() {
        let (_tmp, _g) = sandbox();
        let first = acquire(Duration::from_secs(1)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        let got = acquire(Duration::from_secs(5));
        releaser.join().unwrap();
        assert!(
            got.is_ok(),
            "waiter should acquire once the holder drops: {got:?}"
        );
    }
}
