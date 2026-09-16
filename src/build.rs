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
    /// Discard the checkout's existing build output before configuring.
    pub clean: bool,
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

/// A visible phase of Klipper firmware preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildProgress {
    /// Klipper is expanding the embedded Kconfig.
    Configuring,
    /// Klipper is compiling the firmware artifact.
    Compiling,
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
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The operating system could not spawn or collect the process.
    #[error("could not run build command: {0}")]
    Spawn(#[source] io::Error),
}

/// Executes an explicit build command.
pub trait CommandPort {
    /// Runs `command` and returns its captured output.
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError>;
}

/// Runs commands through the host operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandAdapter;

impl CommandPort for SystemCommandAdapter {
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
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A required path has no parent directory or is not valid UTF-8 for Make.
    #[error("invalid build request: {0}")]
    InvalidRequest(String),
    /// The host filesystem operation failed.
    #[error("could not {action}: {source}")]
    Io {
        /// The operation being attempted.
        action: &'static str,
        /// The underlying filesystem failure.
        #[source]
        source: io::Error,
    },
    /// The command runner could not invoke Make.
    #[error("could not invoke {}: {source}", command.program)]
    CommandPort {
        /// The command that could not be invoked.
        command: BuildCommand,
        /// The underlying runner failure.
        #[source]
        source: CommandError,
    },
    /// Make returned an unsuccessful exit status.
    #[error("{} failed: {}", command.program, String::from_utf8_lossy(&output.stderr).trim())]
    CommandFailed {
        /// The command that failed.
        command: Box<BuildCommand>,
        /// Captured command output for an actionable report.
        output: Box<CommandOutput>,
    },
}

/// Invokes Klipper's Kconfig and firmware build commands through a command runner.
pub struct KlipperBuilder<R> {
    source_dir: PathBuf,
    runner: R,
}

impl<R> KlipperBuilder<R>
where
    R: CommandPort,
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
        self.build_with_progress(request, |_| {})
    }

    /// Builds firmware and reports its externally visible phases.
    pub fn build_with_progress(
        &self,
        request: &BuildRequest,
        mut progress: impl FnMut(BuildProgress),
    ) -> Result<BuildArtifact, BuildError> {
        let config_parent = request.config_path.parent().ok_or_else(|| {
            BuildError::InvalidRequest("config_path must have a parent directory".to_owned())
        })?;
        let artifact_parent = request.artifact_path.parent().ok_or_else(|| {
            BuildError::InvalidRequest("artifact_path must have a parent directory".to_owned())
        })?;

        fs::create_dir_all(config_parent).map_err(|source| BuildError::Io {
            action: "create the temporary Kconfig directory",
            source,
        })?;
        fs::write(&request.config_path, &request.kconfig).map_err(|source| BuildError::Io {
            action: "write the temporary Kconfig",
            source,
        })?;

        // Klipper's Makefile rebuilds board-link/autoconf.h (and drops .d dependency
        // files) whenever KCONFIG_CONFIG's mtime moves, which turns every build into
        // a full rebuild if we hand it a fresh path every run. Writing in place, and
        // only when the content actually changed, keeps `make`'s own incremental
        // tracking effective across repeated updates of the same MCU.
        let checkout_config_path = self.source_dir.join(".config");
        write_if_changed(&checkout_config_path, &request.kconfig).map_err(|source| {
            BuildError::Io {
                action: "write Klipper's build Kconfig",
                source,
            }
        })?;
        let checkout_config_path_str = checkout_config_path.to_str().ok_or_else(|| {
            BuildError::InvalidRequest(
                "the Klipper checkout path must be valid UTF-8 for Make".to_owned(),
            )
        })?;

        if request.clean {
            self.run_make(vec!["clean".to_owned()])?;
        }

        progress(BuildProgress::Configuring);
        self.run_make(vec![
            "olddefconfig".to_owned(),
            kconfig_argument(checkout_config_path_str),
        ])?;
        let expanded_kconfig =
            fs::read_to_string(&checkout_config_path).map_err(|source| BuildError::Io {
                action: "read the expanded Kconfig",
                source,
            })?;
        progress(BuildProgress::Compiling);
        self.run_make(vec![
            kconfig_argument(checkout_config_path_str),
            format!("-j{}", parallel_jobs()),
        ])?;

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
            .map_err(|source| BuildError::CommandPort {
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

/// Writes `content` to `path` only if it differs from what is already there, so an
/// unchanged Kconfig leaves the file's mtime alone and `make` sees nothing to redo.
fn write_if_changed(path: &std::path::Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    fs::write(path, content)
}

fn parallel_jobs() -> std::num::NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(std::num::NonZeroUsize::MIN)
}
