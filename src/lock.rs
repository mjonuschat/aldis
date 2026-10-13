//! The host-wide lock serializing every operation that flashes firmware or touches a bootloader.

use std::fs::{File, OpenOptions, Permissions, TryLockError};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The one lock file shared by every user, `sudo` run, CLI invocation and agent instance. It is
/// not under `$TMPDIR` or `$XDG_RUNTIME_DIR`, because an SSH session and the systemd agent see
/// different values for both.
pub const LOCK_PATH: &str = "/tmp/aldis.lock";

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
        // Every account shares this file, and whichever creates it, the others must still open it.
        // So open it read-only (flock needs no write access) and without O_CREAT, which
        // fs.protected_regular forbids even to root for another user's file in sticky /tmp.
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .map_err(io_error)?;
                // The umask may have cleared bits. Other users only need read access.
                file.set_permissions(Permissions::from_mode(0o644))
                    .map_err(io_error)?;
                file
            }
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
        Self::try_acquire(&default_lock_path())
    }
}

/// [`LOCK_PATH`] as a path.
pub fn default_lock_path() -> PathBuf {
    PathBuf::from(LOCK_PATH)
}
