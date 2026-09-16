//! Durable, human-readable traces for one updater run.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::build::{BuildCommand, CommandError, CommandOutput, CommandPort};

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

    fn command_started(&self, command: &BuildCommand) {
        let _ =
            self.write(format!("[{}] $ {}\n", timestamp(), display_command(command)).as_bytes());
    }

    fn command_finished(&self, output: &CommandOutput) {
        let outcome = if output.success { "success" } else { "failure" };
        let _ = self.write(b"stdout:\n");
        let _ = self.write_output(&output.stdout);
        let _ = self.write(b"stderr:\n");
        let _ = self.write_output(&output.stderr);
        let _ = self.write(format!("exit: {outcome}\n").as_bytes());
    }

    fn command_failed_to_start(&self, error: &CommandError) {
        let _ = self.write(format!("command error: {}\n", crate::error_chain(error)).as_bytes());
    }

    fn write_output(&self, output: &[u8]) -> io::Result<()> {
        self.write(output)?;
        if !output.ends_with(b"\n") {
            self.write(b"\n")?;
        }
        Ok(())
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

/// A command runner that mirrors every command and its complete captured output into a run log.
#[derive(Clone)]
pub struct LoggingCommandAdapter<R> {
    inner: R,
    log: RunLog,
}

impl<R> LoggingCommandAdapter<R> {
    /// Wraps `inner` so its command trace is retained in `log`.
    pub fn new(inner: R, log: RunLog) -> Self {
        Self { inner, log }
    }
}

impl<R: CommandPort> CommandPort for LoggingCommandAdapter<R> {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.log.command_started(command);
        match self.inner.run(command) {
            Ok(output) => {
                self.log.command_finished(&output);
                Ok(output)
            }
            Err(error) => {
                self.log.command_failed_to_start(&error);
                Err(error)
            }
        }
    }
}

fn display_command(command: &BuildCommand) -> String {
    let program = shell_token(&command.program);
    let arguments = command
        .arguments
        .iter()
        .map(|argument| shell_token(argument))
        .collect::<Vec<_>>();
    let invocation = std::iter::once(program)
        .chain(arguments)
        .collect::<Vec<_>>()
        .join(" ");
    command
        .current_dir
        .as_ref()
        .map_or(invocation.clone(), |directory| {
            format!(
                "(cd {} && {invocation})",
                shell_token(&directory.to_string_lossy())
            )
        })
}

fn shell_token(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b',')
        })
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
