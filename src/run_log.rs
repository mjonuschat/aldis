//! Durable, human-readable traces for one updater run.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A persistent, append-only record of one updater run.
#[derive(Clone, Debug)]
pub struct RunLog {
    path: PathBuf,
    file: Arc<Mutex<File>>,
}

/// Errors while creating the durable record for an updater run.
#[derive(Debug, thiserror::Error)]
pub enum RunLogError {
    /// The log file could not be created in the run workspace.
    #[error("could not create run log {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl RunLog {
    /// Creates the `run.log` record retained in a run workspace.
    pub fn create(workspace: &Path) -> Result<Self, RunLogError> {
        let path = workspace.join("run.log");
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|source| RunLogError::Create {
                path: path.clone(),
                source,
            })?;
        let log = Self {
            path,
            file: Arc::new(Mutex::new(file)),
        };
        log.action("run started");
        log.write(
            b"log format: actions, commands, complete stdout, complete stderr, and outcomes\n",
        )
        .ok();
        Ok(log)
    }

    /// Returns the retained trace path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records a user-meaningful action in the current run.
    pub fn action(&self, action: &str) {
        let _ = self.write(format!("[{}] action: {action}\n", timestamp()).as_bytes());
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("run log writer lock poisoned"))?;
        file.write_all(bytes)?;
        file.flush()
    }
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
