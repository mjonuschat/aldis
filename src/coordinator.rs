use std::cell::Cell;
use std::fmt;
use std::path::PathBuf;

use crate::build::{BuildArtifact, BuildError, BuildProgress, CommandPort, KlipperBuilder};
use crate::flash::FlashResult;
use crate::flash::system::{
    SystemFlashError, SystemFlashOptions, SystemFlashProgress,
    flash_prepared_firmware_file_with_progress, flash_prepared_system_with_progress,
};
use crate::moonraker::McuInventory;
use crate::prepare::{PreparationError, PreparedBuild, prepare_build};
use crate::service::{KlipperService, ServiceError, ServiceState};
use crate::workspace::{RunWorkspace, WorkspaceError};

/// A prepared build that is not yet authorized to stop Klipper or run Make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBuild {
    prepared: PreparedBuild,
}

impl PendingBuild {
    /// Returns the selected MCU name for an operator-facing confirmation prompt.
    pub fn target_name(&self) -> &str {
        &self.prepared.target_name
    }

    /// Returns the artifact path that would be produced by the build.
    pub fn artifact_path(&self) -> &PathBuf {
        &self.prepared.request.artifact_path
    }

    /// Returns the Kconfig the selected MCU reported.
    pub fn kconfig(&self) -> &str {
        &self.prepared.request.kconfig
    }

    /// Marks this exact target as approved after an operator has confirmed the side effects.
    pub fn approve(self) -> ApprovedBuild {
        ApprovedBuild { pending: self }
    }
}

/// A target-bound approval permitting the coordinator to stop Klipper and build firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedBuild {
    pending: PendingBuild,
}

/// Errors while preparing or executing a selected MCU build.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// The isolated workspace could not reserve target paths.
    #[error("workspace preparation failed")]
    Workspace(#[source] WorkspaceError),
    /// The selected target could not be matched to Moonraker inventory.
    #[error("build preparation failed")]
    Preparation(#[source] PreparationError),
    /// Klipper's service state could not be queried or changed.
    #[error("Klipper service operation failed")]
    Service(#[source] ServiceError),
    /// Klipper is not in a state that permits a controlled build transition.
    #[error("Klipper must be active or inactive, found {0:?}")]
    UnexpectedKlipperState(ServiceState),
    /// Klipper's build pipeline failed.
    #[error("Klipper build failed")]
    Build(#[source] BuildError),
}

/// Failure while executing an approved build and its caller-supplied flash operation.
#[derive(Debug)]
pub enum FlashCoordinatorError<E> {
    /// The service transition or Klipper build failed.
    Coordinator(CoordinatorError),
    /// The completed artifact could not be read for flashing.
    Artifact(std::io::Error),
    /// The selected native backend failed.
    Flash(E),
}

impl FlashCoordinatorError<SystemFlashError> {
    /// Whether Klipper can safely be restarted after this failure. A USB MCU may
    /// be left in its bootloader, but the host MCU never is.
    pub fn allows_klipper_restore(&self) -> bool {
        match self {
            Self::Flash(error) => matches!(error, SystemFlashError::LinuxHost(_)),
            Self::Coordinator(_) | Self::Artifact(_) => true,
        }
    }
}

impl fmt::Display for FlashCoordinatorError<SystemFlashError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordinator(error) => write!(f, "{error}"),
            Self::Artifact(_) => write!(f, "could not read the built firmware"),
            Self::Flash(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FlashCoordinatorError<SystemFlashError> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Coordinator(error) => std::error::Error::source(error),
            Self::Artifact(error) => Some(error),
            Self::Flash(_) => None,
        }
    }
}

/// The completed build and native flash result for one approved MCU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedUpdate {
    /// The immutable firmware artifact built by Klipper.
    pub artifact: BuildArtifact,
    /// Protocol-reported transfer details.
    pub flash: FlashResult,
}

/// A visible phase of one accepted MCU update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateProgress {
    /// Klipper is stopping before the first build in the batch.
    StoppingKlipper,
    /// Klipper is expanding the selected MCU configuration.
    ConfiguringFirmware,
    /// Klipper is compiling the selected firmware artifact.
    CompilingFirmware,
    /// The updater is entering the selected MCU's bootloader.
    EnteringBootloader,
    /// The expected bootloader has re-enumerated and is ready for its transfer.
    BootloaderReady,
    /// The firmware transfer is beginning.
    StartingFlash,
    /// The Linux host MCU binary is being installed.
    Installing,
}

/// Coordinates the explicit Klipper service boundary and a selected firmware build.
pub struct BuildCoordinator<B, S> {
    builder: KlipperBuilder<B>,
    service: KlipperService<S>,
    first_observed_active: Cell<Option<bool>>,
}

impl<B, S> BuildCoordinator<B, S>
where
    B: CommandPort,
    S: CommandPort,
{
    /// Creates a coordinator rooted at a checked-out Klipper source tree.
    pub fn new(klipper_source_dir: impl Into<PathBuf>, build_runner: B, service_runner: S) -> Self {
        Self {
            builder: KlipperBuilder::new(klipper_source_dir, build_runner),
            service: KlipperService::new(service_runner),
            first_observed_active: Cell::new(None),
        }
    }

    /// Reserves workspace paths and derives a build request without service or Make calls.
    pub fn prepare(
        &self,
        inventory: &McuInventory,
        workspace: &RunWorkspace,
        target_name: &str,
        clean: bool,
    ) -> Result<PendingBuild, CoordinatorError> {
        let paths = workspace
            .build_paths(target_name)
            .map_err(CoordinatorError::Workspace)?;
        let prepared = prepare_build(
            inventory,
            target_name,
            paths.config_path,
            paths.artifact_path,
            clean,
        )
        .map_err(CoordinatorError::Preparation)?;

        Ok(PendingBuild { prepared })
    }

    /// Stops active Klipper, confirms it is inactive, and builds the approved target.
    ///
    /// This never starts Klipper after the build; restart remains an explicit later action.
    pub fn execute(&self, approved: ApprovedBuild) -> Result<BuildArtifact, CoordinatorError> {
        self.execute_with_progress(approved, |_| {})
    }

    /// Stops Klipper if necessary and builds an approved target while reporting progress.
    pub fn execute_with_progress(
        &self,
        approved: ApprovedBuild,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<BuildArtifact, CoordinatorError> {
        self.ensure_klipper_stopped(&mut progress)?;

        self.builder
            .build_with_progress(&approved.pending.prepared.request, |stage| match stage {
                BuildProgress::Configuring => progress(UpdateProgress::ConfiguringFirmware),
                BuildProgress::Compiling => progress(UpdateProgress::CompilingFirmware),
            })
            .map_err(CoordinatorError::Build)
    }

    fn ensure_klipper_stopped(
        &self,
        progress: &mut impl FnMut(UpdateProgress),
    ) -> Result<(), CoordinatorError> {
        let state = self.service.state().map_err(CoordinatorError::Service)?;
        if self.first_observed_active.get().is_none() {
            self.first_observed_active
                .set(Some(state == ServiceState::Active));
        }
        match state {
            ServiceState::Active => {
                progress(UpdateProgress::StoppingKlipper);
                self.service.stop().map_err(CoordinatorError::Service)?;
                match self.service.state().map_err(CoordinatorError::Service)? {
                    ServiceState::Inactive => Ok(()),
                    state => Err(CoordinatorError::UnexpectedKlipperState(state)),
                }
            }
            ServiceState::Inactive => Ok(()),
            state => Err(CoordinatorError::UnexpectedKlipperState(state)),
        }
    }

    /// Stops Klipper if necessary, then flashes caller-supplied firmware bytes directly,
    /// skipping Klipper's build pipeline and its Kconfig validation entirely.
    ///
    /// For an STM32 DFU target, the flash offset is derived from the firmware
    /// file's own vector table and cross-checked against the device's
    /// currently flashed application; a mismatch is refused (and the device
    /// rebooted back into that application) unless `force` overrides it. See
    /// [`crate::flash::system::flash_prepared_firmware_file_with_progress`].
    ///
    /// This preserves the same service boundary as
    /// [`Self::execute_and_flash_system_with_progress`]: Klipper is stopped before the
    /// flash and is never restarted by this operation.
    pub fn flash_firmware_system_with_progress(
        &self,
        approved: ApprovedBuild,
        firmware: &[u8],
        options: SystemFlashOptions,
        force: bool,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<FlashResult, FlashCoordinatorError<SystemFlashError>> {
        self.ensure_klipper_stopped(&mut progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        flash_prepared_firmware_file_with_progress(
            &approved.pending.prepared,
            firmware,
            options,
            self.service.runner(),
            force,
            |stage| {
                progress(match stage {
                    SystemFlashProgress::EnteringBootloader => UpdateProgress::EnteringBootloader,
                    SystemFlashProgress::BootloaderReady => UpdateProgress::BootloaderReady,
                    SystemFlashProgress::Flashing => UpdateProgress::StartingFlash,
                    SystemFlashProgress::Installing => UpdateProgress::Installing,
                });
            },
        )
        .map_err(FlashCoordinatorError::Flash)
    }

    /// Builds an approved target, then invokes `flash` only after Klipper is stopped.
    pub fn execute_and_flash<E>(
        &self,
        approved: ApprovedBuild,
        flash: impl FnOnce(&PreparedBuild, &[u8]) -> Result<FlashResult, E>,
    ) -> Result<CompletedUpdate, FlashCoordinatorError<E>> {
        self.execute_and_flash_with_progress(approved, flash, |_| {})
    }

    /// Builds and flashes an approved target while reporting each visible phase.
    pub fn execute_and_flash_with_progress<E>(
        &self,
        approved: ApprovedBuild,
        flash: impl FnOnce(&PreparedBuild, &[u8]) -> Result<FlashResult, E>,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<CompletedUpdate, FlashCoordinatorError<E>> {
        let mut prepared = approved.pending.prepared.clone();
        let artifact = self
            .execute_with_progress(approved, &mut progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        prepared.request.kconfig = artifact.kconfig.clone();
        let firmware = std::fs::read(&artifact.path).map_err(FlashCoordinatorError::Artifact)?;
        let flash = flash(&prepared, &firmware).map_err(FlashCoordinatorError::Flash)?;
        Ok(CompletedUpdate { artifact, flash })
    }

    /// Builds and flashes through native backends while reporting each visible phase.
    ///
    /// This preserves [`Self::execute_and_flash`]'s service boundary: Klipper is
    /// stopped before the build and is never restarted by this operation.
    pub fn execute_and_flash_system_with_progress(
        &self,
        approved: ApprovedBuild,
        options: SystemFlashOptions,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<CompletedUpdate, FlashCoordinatorError<SystemFlashError>> {
        let mut prepared = approved.pending.prepared.clone();
        let artifact = self
            .execute_with_progress(approved, &mut progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        prepared.request.kconfig = artifact.kconfig.clone();
        let firmware = std::fs::read(&artifact.path).map_err(FlashCoordinatorError::Artifact)?;
        let flash = flash_prepared_system_with_progress(
            &prepared,
            &firmware,
            options,
            self.service.runner(),
            |stage| {
                progress(match stage {
                    SystemFlashProgress::EnteringBootloader => UpdateProgress::EnteringBootloader,
                    SystemFlashProgress::BootloaderReady => UpdateProgress::BootloaderReady,
                    SystemFlashProgress::Flashing => UpdateProgress::StartingFlash,
                    SystemFlashProgress::Installing => UpdateProgress::Installing,
                });
            },
        )
        .map_err(FlashCoordinatorError::Flash)?;
        Ok(CompletedUpdate { artifact, flash })
    }

    /// Starts Klipper once every selected MCU has completed its flash and application checks.
    pub fn start_after_batch(&self) -> Result<(), CoordinatorError> {
        self.start_and_verify()
    }

    /// Restores Klipper after a failure that happened before any bootloader entry (the
    /// initial stop itself, or a Make/build failure) — never call this after a flash
    /// attempt, since the MCU's state at that point is no longer known to be safe to
    /// resume against.
    ///
    /// Starts Klipper only if it was active the first time this coordinator observed
    /// its state; otherwise this is a no-op, so an operator who had already stopped
    /// Klipper before running the batch is left with it stopped, not force-started.
    pub fn restore_after_failure(&self) -> Result<(), CoordinatorError> {
        if self.first_observed_active.get() != Some(true) {
            return Ok(());
        }
        self.start_and_verify()
    }

    fn start_and_verify(&self) -> Result<(), CoordinatorError> {
        self.service.start().map_err(CoordinatorError::Service)?;
        match self.service.state().map_err(CoordinatorError::Service)? {
            ServiceState::Active => Ok(()),
            state => Err(CoordinatorError::UnexpectedKlipperState(state)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{BuildCoordinator, FlashCoordinatorError, PendingBuild, UpdateProgress};
    use crate::build::{BuildCommand, BuildRequest, CommandError, CommandOutput, CommandPort};
    use crate::flash::katapult::system::SystemKatapultOptions;
    use crate::flash::system::{SystemFlashError, SystemFlashOptions};
    use crate::prepare::PreparedBuild;

    #[test]
    fn allows_restoring_klipper_after_a_host_mcu_failure_but_not_a_bootloader_failure() {
        use crate::flash::linux_host::LinuxHostError;

        assert!(
            FlashCoordinatorError::Flash(SystemFlashError::LinuxHost(LinuxHostError::NotHostElf))
                .allows_klipper_restore()
        );
        assert!(
            !FlashCoordinatorError::Flash(SystemFlashError::MissingTransport)
                .allows_klipper_restore()
        );
        assert!(
            FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other("gone"))
                .allows_klipper_restore()
        );
    }

    #[derive(Clone)]
    struct FakeRunner {
        commands: Arc<Mutex<Vec<BuildCommand>>>,
        outputs: Arc<Mutex<VecDeque<CommandOutput>>>,
    }

    impl FakeRunner {
        fn new(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                outputs: Arc::new(Mutex::new(outputs.into_iter().collect())),
            }
        }
    }

    impl CommandPort for FakeRunner {
        fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            self.commands
                .lock()
                .expect("runner lock")
                .push(command.clone());
            self.outputs
                .lock()
                .expect("runner lock")
                .pop_front()
                .ok_or_else(|| CommandError::Spawn(std::io::Error::other("missing fake output")))
        }
    }

    fn success_output() -> CommandOutput {
        CommandOutput::success()
    }

    fn state_output(success: bool, stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn pending_without_transport() -> PendingBuild {
        PendingBuild {
            prepared: PreparedBuild {
                target_name: "mcu".to_owned(),
                mcu: "stm32f429xx".to_owned(),
                transport: None,
                request: BuildRequest {
                    kconfig: String::new(),
                    config_path: PathBuf::new(),
                    artifact_path: PathBuf::new(),
                    clean: false,
                },
            },
        }
    }

    fn flash_options() -> SystemFlashOptions {
        SystemFlashOptions {
            katapult: SystemKatapultOptions {
                baud_rate: 250_000,
                bootloader_timeout: Duration::from_secs(10),
                poll_interval: Duration::from_millis(50),
                read_timeout: Duration::from_secs(5),
                can_bootloader_settle: Duration::from_millis(100),
            },
            host_mcu_unit_file: PathBuf::from("/nonexistent/klipper-mcu.service"),
        }
    }

    #[test]
    fn flash_firmware_stops_klipper_then_dispatches_without_building() {
        let build_runner = FakeRunner::new([]);
        let service_runner = FakeRunner::new([
            state_output(true, b"active\n"),
            success_output(),
            state_output(false, b"inactive\n"),
        ]);
        let coordinator =
            BuildCoordinator::new("/nonexistent", build_runner.clone(), service_runner.clone());
        let mut phases = Vec::new();

        let result = coordinator.flash_firmware_system_with_progress(
            pending_without_transport().approve(),
            b"firmware",
            flash_options(),
            false,
            |phase| phases.push(phase),
        );

        assert!(matches!(
            result,
            Err(FlashCoordinatorError::Flash(
                SystemFlashError::MissingTransport
            ))
        ));
        assert_eq!(phases, [UpdateProgress::StoppingKlipper]);
        assert!(
            build_runner
                .commands
                .lock()
                .expect("runner lock")
                .is_empty()
        );
        assert_eq!(
            service_runner.commands.lock().expect("runner lock").len(),
            3
        );
    }
}
