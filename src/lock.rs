//! The host-wide lock serializing every operation that flashes firmware or touches a bootloader.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Held for the duration of one firmware operation; dropping it releases the lock.
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

/// Why the update lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another update, flash, or reboot holds the lock.
    #[error("another aldis update, flash, or reboot is already running")]
    Busy,
    /// The lock file could not be opened or locked.
    #[error("could not open the update lock at {}", path.display())]
    Io {
        /// The lock file path.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

impl UpdateLock {
    /// Takes the lock at `path` without waiting.
    pub fn try_acquire(path: &Path) -> Result<Self, LockError> {
        let io_error = |source: io::Error| LockError::Io {
            path: path.to_owned(),
            source,
        };
        // A sudo run and the invoking user share this file, so whichever creates it, the other must
        // still open it: open read-only (flock needs no write access) and without O_CREAT, which
        // fs.protected_regular forbids even to root for another user's file in sticky /tmp.
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(io_error)?,
            Err(error) => return Err(io_error(error)),
        };
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(LockError::Busy),
            Err(TryLockError::Error(source)) => Err(io_error(source)),
        }
    }

    /// Takes the lock at [`default_lock_path`] without waiting.
    pub fn try_acquire_default() -> Result<Self, LockError> {
        let path = default_lock_path().map_err(|source| LockError::Io {
            path: PathBuf::from("/proc/self"),
            source,
        })?;
        Self::try_acquire(&path)
    }
}

/// `/tmp/aldis-<uid>.lock`, where a root process started through sudo uses the invoking user's
/// uid so `sudo aldis …` contends with that user's agent. Deliberately not `$TMPDIR` or
/// `$XDG_RUNTIME_DIR`: an SSH session and the systemd agent see different values for both.
pub fn default_lock_path() -> io::Result<PathBuf> {
    let euid = std::fs::metadata("/proc/self")?.uid();
    Ok(lock_path_for(
        euid,
        std::env::var("SUDO_UID").ok().as_deref(),
    ))
}

/// The lock path for effective uid `euid`, honoring `SUDO_UID` only when running as root.
pub fn lock_path_for(euid: u32, sudo_uid: Option<&str>) -> PathBuf {
    let uid = match (euid, sudo_uid.and_then(|uid| uid.parse::<u32>().ok())) {
        (0, Some(invoking)) => invoking,
        _ => euid,
    };
    PathBuf::from(format!("/tmp/aldis-{uid}.lock"))
}
