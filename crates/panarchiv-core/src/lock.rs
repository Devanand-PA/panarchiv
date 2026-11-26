//! Cross-process locking for the archive.
//!
//! The archive can be touched by several processes at once: the desktop
//! app, the CLI, and the P2P server thread all read and write the same
//! `manifest.json`. Without coordination, two writers each start from their
//! own snapshot of the manifest and the second `save()` silently discards
//! the first one's changes — which is how a tag edit could vanish.
//!
//! [`ArchiveLock`] takes an exclusive advisory lock on a lock file using
//! `flock` (Unix) / `LockFileEx` (Windows) via the `fs2` crate. The lock is
//! released automatically when the guard is dropped, including on panic.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::error::{msg, Result};

/// An exclusive lock over one archive directory.
pub struct ArchiveLock {
    file: File,
    path: PathBuf,
}

impl ArchiveLock {
    /// Block until the lock for `root` is acquired.
    ///
    /// The wait is bounded so a crashed peer cannot wedge the app forever;
    /// after `timeout` the lock is force-broken and taken.
    pub fn acquire(root: &Path, timeout: Duration) -> Result<Self> {
        let path = root.join(".archive.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(crate::error::ArchiveError::Io)?;

        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    return Ok(Self { file, path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        // The previous owner died without unlocking. Break the
                        // lock rather than hanging the UI forever.
                        let _ = FileExt::unlock(&file);
                        file.lock_exclusive()
                            .map_err(crate::error::ArchiveError::Io)?;
                        return Ok(Self { file, path });
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(crate::error::ArchiveError::Io(e)),
            }
        }
    }

    /// The path of the lock file (useful for diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ArchiveLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Convenience: acquire the lock for `root` with the default timeout.
pub fn lock_archive(root: &Path) -> Result<ArchiveLock> {
    ArchiveLock::acquire(root, Duration::from_secs(10))
}

/// Fail loudly if the lock cannot be taken, for callers that must not block.
pub fn try_lock_archive(root: &Path) -> Result<ArchiveLock> {
    ArchiveLock::acquire(root, Duration::from_millis(0)).map_err(|_| msg("the archive is busy"))
}
