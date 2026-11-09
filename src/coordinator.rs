use std::cell::Cell;
use std::fmt;
use std::path::PathBuf;

use crate::build::{
    BuildArtifact, BuildError, BuildProgress, BuildRequest, CommandPort, KlipperBuilder,
};
use crate::eligibility::CheckoutRevision;
use crate::flash::FlashResult;
use crate::flash::request::{FlashRequest, Protocol};
use crate::flash::system::{
    SystemFlashError, SystemFlashOptions, SystemFlashProgress, flash_beacon_with_progress,
    flash_prebuilt_katapult_with_progress, flash_prepared_firmware_file_with_progress,
    flash_prepared_system_with_progress,
};
use crate::moonraker::McuInventory;
use crate::prepare::{PreparationError, PreparedBuild, prepare_build};
use crate::service::{KlipperService, KlipperUnit, ServiceError, ServiceState};
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

    /// Returns the path the build will copy its artifact to, before Klipper's extension is added.
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
    /// The firmware is for the Bondtech INDX, and this Klipper has no INDX driver, nor is there an
    /// `indx_klipper` checkout to supply it.
    #[error(
        "Klipper has no INDX driver. Clone https://github.com/BondtechAB/indx_klipper and run its \
         install.sh, or set {} to its location. Kalico needs neither.",
        crate::indx_klipper::SOURCE_ENV
    )]
    IndxSourceMissing,
    /// `indx_klipper` could not make the patched copy of Klipper that INDX firmware is built in.
    #[error(
        "indx_klipper could not prepare this Klipper for INDX firmware. Its patch probably does \
         not apply to this revision:\n{output}"
    )]
    IndxPrepare {
        /// What the script and the patch tool printed.
        output: String,
    },
    /// A request to flash prebuilt firmware was prepared for another MCU than the approved one.
    #[error("the firmware was prepared for {requested}, but {approved} was approved")]
    RequestForAnotherMcu {
        /// The MCU the request was prepared for.
        requested: String,
        /// The MCU that was approved.
        approved: String,
    },
}

/// Failure while executing an approved build and flashing its result.
#[derive(Debug)]
pub enum FlashCoordinatorError {
    /// The service transition or Klipper build failed.
    Coordinator(CoordinatorError),
    /// The completed artifact could not be read for flashing.
    Artifact(std::io::Error),
    /// The selected native backend failed.
    Flash(SystemFlashError),
}

impl FlashCoordinatorError {
    /// Whether Klipper can safely be restarted after this failure. A USB MCU may
    /// be left in its bootloader, but the host MCU never is, and an MCU on a transport prebuilt
    /// or Beacon firmware is not sent over was never asked to enter one.
    pub fn allows_klipper_restore(&self) -> bool {
        match self {
            Self::Flash(error) => matches!(
                error,
                SystemFlashError::LinuxHost(_) | SystemFlashError::LinkRefused { .. }
            ),
            Self::Coordinator(_) | Self::Artifact(_) => true,
        }
    }
}

impl fmt::Display for FlashCoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordinator(error) => write!(f, "{error}"),
            Self::Artifact(_) => write!(f, "could not read the built firmware"),
            Self::Flash(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FlashCoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Coordinator(error) => std::error::Error::source(error),
            Self::Artifact(error) => Some(error),
            Self::Flash(error) => Some(error),
        }
    }
}

/// The update-level phase a native flash stage corresponds to.
fn update_progress(stage: SystemFlashProgress) -> UpdateProgress {
    match stage {
        SystemFlashProgress::EnteringBootloader => UpdateProgress::EnteringBootloader,
        SystemFlashProgress::BootloaderReady => UpdateProgress::BootloaderReady,
        SystemFlashProgress::Flashing => UpdateProgress::StartingFlash,
        SystemFlashProgress::Installing => UpdateProgress::Installing,
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
    /// A prebuilt firmware image is being downloaded.
    DownloadingFirmware,
    /// The prebuilt firmware image was downloaded and verified.
    FirmwareDownloaded,
    /// A firmware image is being read from a local checkout.
    ReadingFirmware,
    /// The local firmware image was read and verified.
    FirmwareRead,
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
    indx_klipper: Option<PathBuf>,
}

impl<B, S> BuildCoordinator<B, S>
where
    B: CommandPort,
    S: CommandPort,
{
    /// Creates a coordinator rooted at a checked-out Klipper source tree.
    pub fn new(klipper_source_dir: impl Into<PathBuf>, build_runner: B, service_runner: S) -> Self {
        Self::with_unit(
            klipper_source_dir,
            build_runner,
            service_runner,
            KlipperUnit::default(),
        )
    }

    /// Creates a coordinator that controls the Klipper service `unit`.
    pub fn with_unit(
        klipper_source_dir: impl Into<PathBuf>,
        build_runner: B,
        service_runner: S,
        unit: KlipperUnit,
    ) -> Self {
        Self {
            builder: KlipperBuilder::new(klipper_source_dir, build_runner),
            service: KlipperService::with_unit(service_runner, unit),
            first_observed_active: Cell::new(None),
            indx_klipper: None,
        }
    }

    /// Builds INDX firmware with the `indx_klipper` checkout at `path`, instead of looking for one.
    pub fn with_indx_klipper(mut self, path: impl Into<PathBuf>) -> Self {
        self.indx_klipper = Some(path.into());
        self
    }

    /// The `indx_klipper` checkout `request` is built with, when its firmware is for the INDX and
    /// this Klipper has no INDX driver of its own.
    fn indx_overlay(&self, request: &BuildRequest) -> Result<Option<PathBuf>, CoordinatorError> {
        let source = self.builder.source_dir();
        if !crate::indx_klipper::needs_overlay(&request.kconfig, source) {
            return Ok(None);
        }
        self.indx_klipper
            .clone()
            .or_else(|| crate::indx_klipper::find_checkout(source))
            .map(Some)
            .ok_or(CoordinatorError::IndxSourceMissing)
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
        let request = &approved.pending.prepared.request;
        let indx_tree = self
            .indx_overlay(request)?
            .map(|checkout| self.prepare_indx_tree(&checkout))
            .transpose()?;
        self.ensure_klipper_stopped(&mut progress)?;

        let on_stage = |stage| match stage {
            BuildProgress::Configuring => progress(UpdateProgress::ConfiguringFirmware),
            BuildProgress::Compiling => progress(UpdateProgress::CompilingFirmware),
        };
        match indx_tree {
            Some(tree) => self
                .builder
                .in_tree(tree)
                .build_with_progress(request, on_stage),
            None => self.builder.build_with_progress(request, on_stage),
        }
        .map_err(CoordinatorError::Build)
    }

    /// Makes the patched copy of Klipper that INDX firmware is built in, and gives it Klipper's
    /// version. Klipper keeps running meanwhile, so a patch that does not apply leaves it alone.
    fn prepare_indx_tree(&self, checkout: &std::path::Path) -> Result<PathBuf, CoordinatorError> {
        self.builder
            .run_step(crate::indx_klipper::prepare_command(
                checkout,
                self.builder.source_dir(),
            ))
            .map_err(|error| match error {
                BuildError::CommandFailed { output, .. } => CoordinatorError::IndxPrepare {
                    output: crate::indx_klipper::report(&output.stdout, &output.stderr),
                },
                error => CoordinatorError::Build(error),
            })?;
        let tree = crate::indx_klipper::build_dir(checkout);
        if let Ok(CheckoutRevision::Known(revision)) =
            crate::checkout::revision(self.builder.source_dir())
        {
            crate::indx_klipper::write_version(&tree, &revision).map_err(|source| {
                CoordinatorError::Build(BuildError::Io {
                    action: "give the INDX build Klipper's version",
                    source,
                })
            })?;
        }
        Ok(tree)
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
    ) -> Result<FlashResult, FlashCoordinatorError> {
        self.ensure_klipper_stopped(&mut progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        flash_prepared_firmware_file_with_progress(
            &approved.pending.prepared,
            firmware,
            options,
            self.service.runner(),
            force,
            |stage| progress(update_progress(stage)),
        )
        .map_err(FlashCoordinatorError::Flash)
    }

    /// Stops Klipper if necessary, then flashes prebuilt firmware to the approved MCU by the
    /// protocol its request names (see [`flash_prebuilt_katapult_with_progress`] and
    /// [`flash_beacon_with_progress`]). A request prepared for another MCU than the approved one
    /// is refused before Klipper is stopped. Like every flash, this never restarts Klipper.
    pub fn flash_prebuilt(
        &self,
        approved: ApprovedBuild,
        request: &FlashRequest,
        options: SystemFlashOptions,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<FlashResult, FlashCoordinatorError> {
        let prepared = &approved.pending.prepared;
        if !request.destination().is(prepared) {
            return Err(FlashCoordinatorError::Coordinator(
                CoordinatorError::RequestForAnotherMcu {
                    requested: request.destination().name().to_owned(),
                    approved: prepared.target_name.clone(),
                },
            ));
        }
        self.ensure_klipper_stopped(&mut progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        let progress = |stage| progress(update_progress(stage));
        match request.protocol() {
            Protocol::Katapult {
                image,
                application_start,
                links,
            } => flash_prebuilt_katapult_with_progress(
                prepared,
                image,
                *application_start,
                *links,
                options,
                progress,
            ),
            Protocol::BeaconDfu { image } => {
                flash_beacon_with_progress(prepared, image, options, progress)
            }
        }
        .map_err(FlashCoordinatorError::Flash)
    }

    /// Builds an approved target, then invokes `flash` only after Klipper is stopped.
    pub fn execute_and_flash(
        &self,
        approved: ApprovedBuild,
        flash: impl FnOnce(&PreparedBuild, &[u8]) -> Result<FlashResult, SystemFlashError>,
    ) -> Result<CompletedUpdate, FlashCoordinatorError> {
        self.execute_and_flash_with_progress(approved, flash, |_| {})
    }

    /// Builds and flashes an approved target while reporting each visible phase.
    pub fn execute_and_flash_with_progress(
        &self,
        approved: ApprovedBuild,
        flash: impl FnOnce(&PreparedBuild, &[u8]) -> Result<FlashResult, SystemFlashError>,
        mut progress: impl FnMut(UpdateProgress),
    ) -> Result<CompletedUpdate, FlashCoordinatorError> {
        let (prepared, artifact, firmware) = self.build_for_flash(approved, &mut progress)?;
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
    ) -> Result<CompletedUpdate, FlashCoordinatorError> {
        let (prepared, artifact, firmware) = self.build_for_flash(approved, &mut progress)?;
        let flash = flash_prepared_system_with_progress(
            &prepared,
            &firmware,
            options,
            self.service.runner(),
            |stage| progress(update_progress(stage)),
        )
        .map_err(FlashCoordinatorError::Flash)?;
        Ok(CompletedUpdate { artifact, flash })
    }

    /// Stops Klipper if necessary, builds `approved`, and reads the artifact back for flashing,
    /// with the prepared build's Kconfig replaced by the one the build actually used.
    fn build_for_flash(
        &self,
        approved: ApprovedBuild,
        progress: &mut impl FnMut(UpdateProgress),
    ) -> Result<(PreparedBuild, BuildArtifact, Vec<u8>), FlashCoordinatorError> {
        let mut prepared = approved.pending.prepared.clone();
        let artifact = self
            .execute_with_progress(approved, progress)
            .map_err(FlashCoordinatorError::Coordinator)?;
        prepared.request.kconfig = artifact.kconfig.clone();
        let firmware = std::fs::read(&artifact.path).map_err(FlashCoordinatorError::Artifact)?;
        Ok((prepared, artifact, firmware))
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

    const INDX_KCONFIG: &str = "CONFIG_MACH_SAME51J19=y\nCONFIG_WANT_INDX_HEATER=y\n";
    use crate::build::{BuildCommand, BuildRequest, CommandError, CommandOutput, CommandPort};
    use crate::flash::system::SystemKatapultOptions;
    use crate::flash::system::{SystemFlashError, SystemFlashOptions};
    use crate::prepare::PreparedBuild;
    use crate::service::KlipperUnit;

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
            FlashCoordinatorError::Artifact(std::io::Error::other("gone")).allows_klipper_restore()
        );
        assert!(
            FlashCoordinatorError::Flash(SystemFlashError::LinkRefused {
                route: crate::flash::system::PrebuiltRoute::KatapultUsb,
                transport: "CAN (can0, e7819ed8e7d3)".to_owned(),
            })
            .allows_klipper_restore(),
            "an MCU prebuilt firmware is not sent to was never touched"
        );
        assert!(
            !FlashCoordinatorError::Flash(SystemFlashError::NotKatapult {
                bootloader: "STM32 DFU",
                transport: "/dev/ttyACM0".to_owned(),
            })
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

    #[test]
    fn stops_the_configured_klipper_unit() {
        let service_runner = FakeRunner::new([
            state_output(true, b"active\n"),
            success_output(),
            state_output(false, b"inactive\n"),
        ]);
        let coordinator = BuildCoordinator::with_unit(
            "/nonexistent",
            FakeRunner::new([]),
            service_runner.clone(),
            KlipperUnit::parse("klipper-2").unwrap(),
        );

        let _ = coordinator.flash_firmware_system_with_progress(
            pending_without_transport().approve(),
            b"firmware",
            flash_options(),
            false,
            |_| {},
        );

        let commands = service_runner.commands.lock().expect("runner lock");
        assert_eq!(commands.len(), 3);
        assert!(
            commands
                .iter()
                .all(|command| command.arguments.last().map(String::as_str) == Some("klipper-2"))
        );
    }

    #[test]
    fn builds_indx_firmware_in_a_patched_copy_of_klipper() {
        let dir = tempfile::tempdir().unwrap();
        let klipper = dir.path().join("klipper");
        let checkout = dir.path().join("indx_klipper");
        let tree = checkout.join("out/klipper-indx");
        std::fs::create_dir_all(&klipper).unwrap();
        std::fs::create_dir_all(tree.join("out")).unwrap();
        std::fs::write(tree.join("out/klipper.bin"), b"firmware").unwrap();
        let build_runner = FakeRunner::new([success_output(), success_output(), success_output()]);
        let service_runner = FakeRunner::new([state_output(false, b"inactive\n")]);
        let coordinator = BuildCoordinator::new(&klipper, build_runner.clone(), service_runner)
            .with_indx_klipper(&checkout);
        let mut pending = pending_without_transport();
        pending.prepared.request = BuildRequest {
            kconfig: INDX_KCONFIG.to_owned(),
            config_path: dir.path().join("run/.config"),
            artifact_path: dir.path().join("run/klipper"),
            clean: false,
        };
        std::fs::create_dir_all(dir.path().join("run")).unwrap();

        let artifact = coordinator.execute(pending.approve()).unwrap();

        assert_eq!(artifact.path, dir.path().join("run/klipper.bin"));
        let commands = build_runner.commands.lock().expect("runner lock");
        assert_eq!(commands[0].program, "python3");
        assert_eq!(
            commands[0].arguments[1..3],
            [klipper.display().to_string(), tree.display().to_string()]
        );
        assert!(
            commands[1..].iter().all(|command| command.program == "make"
                && command.current_dir.as_deref() == Some(tree.as_path())),
            "make runs in the patched copy, not in the Klipper checkout: {commands:?}"
        );
    }

    #[test]
    fn reports_a_patch_that_does_not_apply_with_what_the_patch_tool_said() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("indx_klipper");
        let failed = CommandOutput {
            success: false,
            stdout: b"1 out of 3 hunks FAILED\n".to_vec(),
            stderr: b"Traceback (most recent call last):\nsubprocess.CalledProcessError: exit 1\n"
                .to_vec(),
        };
        let service_runner = FakeRunner::new([]);
        let coordinator = BuildCoordinator::new(
            dir.path().join("klipper"),
            FakeRunner::new([failed]),
            service_runner.clone(),
        )
        .with_indx_klipper(checkout);
        let mut pending = pending_without_transport();
        pending.prepared.request.kconfig = INDX_KCONFIG.to_owned();

        let error = coordinator.execute(pending.approve()).unwrap_err();

        let super::CoordinatorError::IndxPrepare { output } = &error else {
            panic!("expected the patch failure to be reported as such: {error:?}");
        };
        assert!(output.contains("1 out of 3 hunks FAILED"), "{output}");
        assert!(output.contains("CalledProcessError"), "{output}");
        assert!(
            service_runner
                .commands
                .lock()
                .expect("runner lock")
                .is_empty(),
            "Klipper is not stopped for a build that cannot start"
        );
    }

    #[test]
    fn flashes_a_file_to_an_indx_without_any_indx_klipper_checkout() {
        let service_runner = FakeRunner::new([
            state_output(true, b"active\n"),
            success_output(),
            state_output(false, b"inactive\n"),
        ]);
        let coordinator =
            BuildCoordinator::new("/nonexistent", FakeRunner::new([]), service_runner);
        let mut pending = pending_without_transport();
        pending.prepared.request.kconfig = INDX_KCONFIG.to_owned();

        let result = coordinator.flash_firmware_system_with_progress(
            pending.approve(),
            b"firmware",
            flash_options(),
            false,
            |_| {},
        );

        assert!(
            matches!(
                result,
                Err(FlashCoordinatorError::Flash(
                    SystemFlashError::MissingTransport
                ))
            ),
            "no build runs, so no build source is needed: {result:?}"
        );
    }
}
