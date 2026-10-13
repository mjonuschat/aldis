use crate::build::{BuildCommand, CommandError, CommandOutput, CommandPort};

/// The Klipper unit used when Moonraker does not report one.
pub const DEFAULT_KLIPPER_UNIT: &str = "klipper";

const MAX_UNIT_LEN: usize = 64;

/// A name that is not a valid Klipper service name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a valid Klipper service name")]
pub struct InvalidKlipperUnit(pub String);

/// A validated Klipper systemd unit name: `klipper` followed by `[A-Za-z0-9_.@-]`, at most 64
/// bytes. `klipper@voron` is valid.
///
/// The name is validated before any command is built from it, and the check matches the sudoers
/// rule that `setup` installs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KlipperUnit(String);

impl KlipperUnit {
    /// Validates `name` as a Klipper unit name.
    pub fn parse(name: &str) -> Result<Self, InvalidKlipperUnit> {
        let valid = name.len() <= MAX_UNIT_LEN
            && name.strip_prefix(DEFAULT_KLIPPER_UNIT).is_some_and(|rest| {
                rest.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'@')
                })
            });
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(InvalidKlipperUnit(name.to_owned()))
        }
    }

    /// The unit name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for KlipperUnit {
    fn default() -> Self {
        Self(DEFAULT_KLIPPER_UNIT.to_owned())
    }
}

impl std::fmt::Display for KlipperUnit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A Klipper service state reported by systemd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    /// Klipper is active.
    Active,
    /// Klipper is inactive.
    Inactive,
    /// systemd reports the unit failed.
    Failed,
    /// systemd reports a known transitional state.
    Transitioning(String),
    /// systemd returned an unrecognized state.
    Unknown(String),
}

/// Errors while querying or changing Klipper's systemd service state.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The host could not run a systemctl command.
    #[error("could not invoke {}", command.program)]
    CommandPort {
        /// The command that could not be invoked.
        command: BuildCommand,
        /// The underlying runner failure.
        #[source]
        source: CommandError,
    },
    /// systemctl rejected a requested state change.
    #[error("{} failed: {}", command.program, String::from_utf8_lossy(&output.stderr).trim())]
    CommandFailed {
        /// The systemctl command that failed.
        command: Box<BuildCommand>,
        /// Captured command output.
        output: Box<CommandOutput>,
    },
}

/// A testable systemd controller for Klipper's service lifecycle.
pub struct KlipperService<R> {
    runner: R,
    unit: KlipperUnit,
}

impl<R> KlipperService<R>
where
    R: CommandPort,
{
    /// Creates a controller for the default `klipper` unit using `runner` to invoke systemctl.
    pub fn new(runner: R) -> Self {
        Self::with_unit(runner, KlipperUnit::default())
    }

    /// Creates a controller for `unit` using `runner` to invoke systemctl.
    pub fn with_unit(runner: R, unit: KlipperUnit) -> Self {
        Self { runner, unit }
    }

    /// Returns the unit this controller manages.
    pub fn unit(&self) -> &KlipperUnit {
        &self.unit
    }

    /// Returns the runner used for privileged host commands.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// Returns systemd's current Klipper state without changing it.
    pub fn state(&self) -> Result<ServiceState, ServiceError> {
        let (_, output) = self.run_systemctl(["is-active", self.unit.as_str()])?;
        let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        Ok(match state.as_str() {
            "active" => ServiceState::Active,
            "inactive" => ServiceState::Inactive,
            "failed" => ServiceState::Failed,
            "activating" | "deactivating" | "reloading" => ServiceState::Transitioning(state),
            _ => ServiceState::Unknown(state),
        })
    }

    /// Stops Klipper and returns an error if systemctl rejects the request.
    pub fn stop(&self) -> Result<(), ServiceError> {
        self.run_state_change("stop")
    }

    /// Starts Klipper and returns an error if systemctl rejects the request.
    pub fn start(&self) -> Result<(), ServiceError> {
        self.run_state_change("start")
    }

    fn run_state_change(&self, action: &str) -> Result<(), ServiceError> {
        let (command, output) = self.run_systemctl([action, self.unit.as_str()])?;
        if output.success {
            Ok(())
        } else {
            Err(ServiceError::CommandFailed {
                command: Box::new(command),
                output: Box::new(output),
            })
        }
    }

    fn run_systemctl<const N: usize>(
        &self,
        arguments: [&str; N],
    ) -> Result<(BuildCommand, CommandOutput), ServiceError> {
        let command = BuildCommand {
            program: "sudo".to_owned(),
            arguments: std::iter::once("-n".to_owned())
                .chain(std::iter::once("/bin/systemctl".to_owned()))
                .chain(arguments.into_iter().map(str::to_owned))
                .collect(),
            current_dir: None,
            stdin: None,
        };
        let output = self
            .runner
            .run(&command)
            .map_err(|source| ServiceError::CommandPort {
                command: command.clone(),
                source,
            })?;

        Ok((command, output))
    }
}
