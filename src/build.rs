use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

/// The inputs and output location for one Klipper firmware build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// Embedded Kconfig obtained from Moonraker for the selected MCU.
    pub kconfig: String,
    /// Per-run temporary location for Klipper's generated Kconfig.
    pub config_path: PathBuf,
    /// Destination for the immutable firmware artifact produced by this build.
    pub artifact_path: PathBuf,
}

/// The firmware artifact copied from Klipper's build output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildArtifact {
    /// Path to the copied firmware artifact.
    pub path: PathBuf,
    /// Artifact size in bytes.
    pub byte_count: u64,
    /// Kconfig after Klipper has expanded defaults with `olddefconfig`.
    pub kconfig: String,
}

/// A fully specified command invocation without shell interpolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCommand {
    /// Executable name or path.
    pub program: String,
    /// Explicit command arguments.
    pub arguments: Vec<String>,
    /// Optional directory in which the command runs.
    pub current_dir: Option<PathBuf>,
}

/// Captured result of a command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the process returned a successful exit status.
    pub success: bool,
    /// Complete standard output.
    pub stdout: Vec<u8>,
    /// Complete standard error.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// Produces an empty successful result for deterministic test runners.
    pub fn success() -> Self {
        Self {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

/// Failure to start or wait for a build command.
#[derive(Debug)]
pub enum CommandError {
    /// The operating system could not spawn or collect the process.
    Spawn(io::Error),
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not run build command: {error}"),
        }
    }
}

impl StdError for CommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
        }
    }
}

/// Executes an explicit build command.
pub trait CommandRunner {
    /// Runs `command` and returns its captured output.
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError>;
}

/// Runs commands through the host operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        let mut process = Command::new(&command.program);
        process.args(&command.arguments);
        if let Some(current_dir) = &command.current_dir {
            process.current_dir(current_dir);
        }
        let output = process.output().map_err(CommandError::Spawn)?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Errors while preparing or running Klipper's existing build pipeline.
#[derive(Debug)]
pub enum BuildError {
    /// A required path has no parent directory or is not valid UTF-8 for Make.
    InvalidRequest(String),
    /// The host filesystem operation failed.
    Io {
        /// The operation being attempted.
        action: &'static str,
        /// The underlying filesystem failure.
        source: io::Error,
    },
    /// The command runner could not invoke Make.
    CommandRunner {
        /// The command that could not be invoked.
        command: BuildCommand,
        /// The underlying runner failure.
        source: CommandError,
    },
    /// Make returned an unsuccessful exit status.
    CommandFailed {
        /// The command that failed.
        command: Box<BuildCommand>,
        /// Captured command output for an actionable report.
        output: Box<CommandOutput>,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid build request: {message}"),
            Self::Io { action, source } => write!(formatter, "could not {action}: {source}"),
            Self::CommandRunner { command, source } => {
                write!(formatter, "could not invoke {}: {source}", command.program)
            }
            Self::CommandFailed { command, output } => write!(
                formatter,
                "{} failed: {}",
                command.program,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

impl StdError for BuildError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::CommandRunner { source, .. } => Some(source),
            Self::InvalidRequest(_) | Self::CommandFailed { .. } => None,
        }
    }
}

/// Invokes Klipper's Kconfig and firmware build commands through a command runner.
pub struct KlipperBuilder<R> {
    source_dir: PathBuf,
    runner: R,
}

impl<R> KlipperBuilder<R>
where
    R: CommandRunner,
{
    /// Creates a builder rooted at the checked-out Klipper source tree.
    pub fn new(source_dir: impl Into<PathBuf>, runner: R) -> Self {
        Self {
            source_dir: source_dir.into(),
            runner,
        }
    }

    /// Builds firmware from `request.kconfig` and copies the generated artifact.
    pub fn build(&self, request: &BuildRequest) -> Result<BuildArtifact, BuildError> {
        let config_parent = request.config_path.parent().ok_or_else(|| {
            BuildError::InvalidRequest("config_path must have a parent directory".to_owned())
        })?;
        let artifact_parent = request.artifact_path.parent().ok_or_else(|| {
            BuildError::InvalidRequest("artifact_path must have a parent directory".to_owned())
        })?;
        let config_path = request.config_path.to_str().ok_or_else(|| {
            BuildError::InvalidRequest("config_path must be valid UTF-8 for Make".to_owned())
        })?;

        fs::create_dir_all(config_parent).map_err(|source| BuildError::Io {
            action: "create the temporary Kconfig directory",
            source,
        })?;
        fs::write(&request.config_path, &request.kconfig).map_err(|source| BuildError::Io {
            action: "write the temporary Kconfig",
            source,
        })?;

        self.run_make(vec![
            "olddefconfig".to_owned(),
            kconfig_argument(config_path),
        ])?;
        let expanded_kconfig =
            fs::read_to_string(&request.config_path).map_err(|source| BuildError::Io {
                action: "read the expanded Kconfig",
                source,
            })?;
        self.run_make(vec![kconfig_argument(config_path)])?;

        fs::create_dir_all(artifact_parent).map_err(|source| BuildError::Io {
            action: "create the artifact directory",
            source,
        })?;
        let source_artifact = self
            .source_dir
            .join(output_artifact_name(&expanded_kconfig));
        fs::copy(&source_artifact, &request.artifact_path).map_err(|source| BuildError::Io {
            action: "copy Klipper's firmware artifact",
            source,
        })?;
        let byte_count = fs::metadata(&request.artifact_path)
            .map_err(|source| BuildError::Io {
                action: "read the copied firmware artifact",
                source,
            })?
            .len();

        Ok(BuildArtifact {
            path: request.artifact_path.clone(),
            byte_count,
            kconfig: expanded_kconfig,
        })
    }

    fn run_make(&self, arguments: Vec<String>) -> Result<(), BuildError> {
        let command = BuildCommand {
            program: "make".to_owned(),
            arguments,
            current_dir: Some(self.source_dir.clone()),
        };
        let output = self
            .runner
            .run(&command)
            .map_err(|source| BuildError::CommandRunner {
                command: command.clone(),
                source,
            })?;

        if output.success {
            Ok(())
        } else {
            Err(BuildError::CommandFailed {
                command: Box::new(command),
                output: Box::new(output),
            })
        }
    }
}

fn output_artifact_name(kconfig: &str) -> &'static str {
    if kconfig
        .lines()
        .any(|line| line.trim() == "CONFIG_MACH_RPXXXX=y")
        && !kconfig
            .lines()
            .any(|line| line.trim() == "CONFIG_RPXXXX_FLASH_START_4000=y")
    {
        "out/klipper.uf2"
    } else {
        "out/klipper.bin"
    }
}

fn kconfig_argument(config_path: &str) -> String {
    format!("KCONFIG_CONFIG={config_path}")
}
