// SPDX-License-Identifier: Apache-2.0

//! `solo.lock`: O_EXCL-style mutex that prevents two daemons (or two `solo
//! init` invocations) from racing on the same data dir.
//!
//! Per ADR-0003 §P8-I: stale-lock recovery + PID-alive checks. Behaviour:
//!
//!   - `Lockfile::acquire(path)` creates the file with `O_EXCL` (cross-platform
//!     via `OpenOptions::create_new`) and writes the current PID.
//!   - If the file already exists, we read the PID it contains and ask the OS
//!     whether that PID is currently alive (via `sysinfo`). Two outcomes:
//!       - **Alive**: refuse with `Error::Conflict` — another Solo process
//!         genuinely owns the data dir.
//!       - **Dead** (or unparseable PID): the previous run crashed without
//!         removing the file. Remove it and retry once. We never loop —
//!         repeated failures fall through to the conflict error.
//!   - On drop, the file is deleted. (Best-effort; if delete fails we log
//!     and continue.)
//!
//! Why we want this even in commit 1.1: `solo init` creates a fresh DB with
//! the user's chosen passphrase. If two `solo init` invocations race, they
//! could each generate a different salt and overwrite each other's config —
//! leaving the user with a passphrase that no longer matches the on-disk DB.

use solo_core::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// Cross-platform PID-alive check. Internally, `sysinfo` queries `/proc` on
/// Linux, `kqueue` on BSD/macOS, and `OpenProcess` on Windows. We refresh a
/// fresh `System` each call — there's no global state to keep stale.
fn is_pid_alive(pid: u32) -> bool {
    // Minimal refresh — we only need the process list, not CPU/memory/etc.
    let sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    sys.process(Pid::from_u32(pid)).is_some()
}

/// RAII handle to the data-dir lockfile.
#[derive(Debug)]
pub struct Lockfile {
    path: PathBuf,
    /// Held to keep the OS handle open for the lifetime of the guard. Dropping
    /// closes the handle; we explicitly remove the file in our own Drop impl.
    _handle: File,
}

impl Lockfile {
    /// Acquire the lock by creating `path` with O_EXCL. Writes the current
    /// PID to the file. If the file already exists, attempt stale-lock
    /// recovery: read the persisted PID, ask the OS if it's alive, remove
    /// and retry once if it isn't.
    pub fn acquire(path: &Path) -> Result<Self> {
        match Self::try_create(path) {
            Ok(lf) => Ok(lf),
            Err(Error::Conflict(_)) => {
                // Existing lockfile — investigate.
                Self::try_recover_stale(path)?;
                // One retry. If this fails too, surface the conflict.
                Self::try_create(path)
            }
            Err(e) => Err(e),
        }
    }

    /// Best-effort: if the existing lockfile's PID is dead, remove it.
    /// Returns Ok if recovered, Err(Conflict) if the lock is genuinely held.
    fn try_recover_stale(path: &Path) -> Result<()> {
        let body = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                // Can't read — stay conservative, treat as held.
                return Err(Self::held_error(path, None));
            }
        };
        let pid = body.trim().parse::<u32>().ok();
        let alive = match pid {
            Some(p) => is_pid_alive(p),
            // Unparseable PID body (corruption, partial write); treat as
            // stale and recover.
            None => false,
        };
        if alive {
            return Err(Self::held_error(path, pid));
        }
        // Stale: the previous run died without cleaning up.
        tracing::warn!(
            ?pid,
            path = %path.display(),
            "stale lockfile detected (pid not alive); removing"
        );
        std::fs::remove_file(path).map_err(|e| {
            Error::storage(format!("remove stale lockfile {}: {e}", path.display()))
        })?;
        Ok(())
    }

    fn try_create(path: &Path) -> Result<Self> {
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => Self::held_error(path, None),
                _ => Error::storage(format!("open lockfile {}: {e}", path.display())),
            })?;
        let pid = std::process::id();
        write!(handle, "{pid}")
            .map_err(|e| Error::storage(format!("write pid to lockfile: {e}")))?;
        handle
            .sync_all()
            .map_err(|e| Error::storage(format!("fsync lockfile: {e}")))?;
        Ok(Self {
            path: path.to_path_buf(),
            _handle: handle,
        })
    }

    fn held_error(path: &Path, pid: Option<u32>) -> Error {
        let pid_msg = match pid {
            Some(p) => format!(" (held by pid {p})"),
            None => String::new(),
        };
        Error::conflict(format!(
            "lockfile {} already exists{pid_msg} — another Solo process is \
             running. If you're sure no other instance is alive, remove the \
             file manually.",
            path.display()
        ))
    }

    /// Path to the lockfile (for diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lockfile {
    fn drop(&mut self) {
        // Best-effort: if delete fails (e.g., the file was already removed),
        // log and continue. We deliberately don't panic in Drop.
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                error = %e,
                path = %self.path.display(),
                "failed to remove lockfile on drop"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_creates_file_with_pid() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        let _lock = Lockfile::acquire(&path).unwrap();
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        let pid: u32 = body.parse().expect("pid should be a number");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn second_acquire_fails_with_conflict() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        let _lock = Lockfile::acquire(&path).unwrap();
        let err = Lockfile::acquire(&path).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got: {err:?}");
    }

    #[test]
    fn drop_removes_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        {
            let _lock = Lockfile::acquire(&path).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "lockfile should be removed on drop");
    }

    #[test]
    fn re_acquire_after_drop_succeeds() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        {
            let _lock = Lockfile::acquire(&path).unwrap();
        }
        let _lock2 = Lockfile::acquire(&path).unwrap();
    }

    #[test]
    fn stale_lockfile_with_dead_pid_is_recovered() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        // Plant a stale lockfile with a bogus, definitely-dead PID. PID 1
        // is reserved on Unix (init); we want a number that's vanishingly
        // unlikely to exist. u32::MAX is a safe choice — process IDs are
        // bounded well below that on every supported OS.
        std::fs::write(&path, format!("{}", u32::MAX)).unwrap();
        // Acquire should remove the stale file and create a fresh one with
        // the current PID.
        let lock = Lockfile::acquire(&path).unwrap();
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        let pid: u32 = body.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());
        drop(lock);
    }

    #[test]
    fn stale_lockfile_with_unparseable_body_is_recovered() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        std::fs::write(&path, b"<garbage from a partial write>").unwrap();
        let _lock = Lockfile::acquire(&path).unwrap();
        // No assertion needed beyond Ok — getting here means recovery worked.
    }

    #[test]
    fn live_pid_is_not_recovered() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.lock");
        // Use the current process's PID — definitely alive.
        std::fs::write(&path, format!("{}", std::process::id())).unwrap();
        let err = Lockfile::acquire(&path).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got: {err:?}");
        // The file must still exist (we didn't remove a live lock).
        assert!(path.exists());
    }
}
