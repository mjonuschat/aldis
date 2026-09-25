use crate::build::{BuildCommand, CommandError, CommandOutput, CommandPort};

const KLIPPER_UNIT: &str = "klipper";

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
}

impl<R> KlipperService<R>
where
    R: CommandPort,
{
    /// Creates a controller using `runner` to invoke systemctl.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Returns the runner used for privileged host commands.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// Returns systemd's current Klipper state without changing it.
    pub fn state(&self) -> Result<ServiceState, ServiceError> {
        let (_, output) = self.run_systemctl(["is-active", KLIPPER_UNIT])?;
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
        let (command, output) = self.run_systemctl([action, KLIPPER_UNIT])?;
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
