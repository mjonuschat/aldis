use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use crate::build::{BuildArtifact, BuildError, CommandRunner, KlipperBuilder};
use crate::flash::FlashResult;
use crate::flash::system::{SystemFlashError, SystemFlashOptions, flash_prepared_system};
use crate::moonraker::McuInventory;
use crate::plan::UpdatePlan;
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
#[derive(Debug)]
pub enum CoordinatorError {
    /// The isolated workspace could not reserve target paths.
    Workspace(WorkspaceError),
    /// The selected plan target could not be matched to Moonraker inventory.
    Preparation(PreparationError),
    /// Klipper's service state could not be queried or changed.
    Service(ServiceError),
    /// Klipper is not in a state that permits a controlled build transition.
    UnexpectedKlipperState(ServiceState),
    /// Klipper's build pipeline failed.
    Build(BuildError),
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

/// The completed build and native flash result for one approved MCU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedUpdate {
    /// The immutable firmware artifact built by Klipper.
    pub artifact: BuildArtifact,
    /// Protocol-reported transfer details.
    pub flash: FlashResult,
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(error) => write!(formatter, "workspace preparation failed: {error}"),
            Self::Preparation(error) => write!(formatter, "build preparation failed: {error}"),
            Self::Service(error) => write!(formatter, "Klipper service operation failed: {error}"),
            Self::UnexpectedKlipperState(state) => {
                write!(
                    formatter,
                    "Klipper must be active or inactive, found {state:?}"
                )
            }
            Self::Build(error) => write!(formatter, "Klipper build failed: {error}"),
        }
    }
}

impl StdError for CoordinatorError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
            Self::Preparation(error) => Some(error),
            Self::Service(error) => Some(error),
            Self::Build(error) => Some(error),
            Self::UnexpectedKlipperState(_) => None,
        }
    }
}

/// Coordinates the explicit Klipper service boundary and a selected firmware build.
pub struct BuildCoordinator<B, S> {
    builder: KlipperBuilder<B>,
    service: KlipperService<S>,
}

impl<B, S> BuildCoordinator<B, S>
where
    B: CommandRunner,
    S: CommandRunner,
{
    /// Creates a coordinator rooted at a checked-out Klipper source tree.
    pub fn new(klipper_source_dir: impl Into<PathBuf>, build_runner: B, service_runner: S) -> Self {
        Self {
            builder: KlipperBuilder::new(klipper_source_dir, build_runner),
            service: KlipperService::new(service_runner),
        }
    }

    /// Reserves workspace paths and derives a build request without service or Make calls.
    pub fn prepare(
        &self,
        inventory: &McuInventory,
        plan: &UpdatePlan,
        workspace: &RunWorkspace,
        target_name: &str,
    ) -> Result<PendingBuild, CoordinatorError> {
        let paths = workspace
            .build_paths(target_name)
            .map_err(CoordinatorError::Workspace)?;
        let prepared = prepare_build(
            inventory,
            plan,
            target_name,
            paths.config_path,
            paths.artifact_path,
        )
        .map_err(CoordinatorError::Preparation)?;

        Ok(PendingBuild { prepared })
    }

    /// Stops active Klipper, confirms it is inactive, and builds the approved target.
    ///
    /// This never starts Klipper after the build; restart remains an explicit later action.
    pub fn execute(&self, approved: ApprovedBuild) -> Result<BuildArtifact, CoordinatorError> {
        match self.service.state().map_err(CoordinatorError::Service)? {
            ServiceState::Active => {
                self.service.stop().map_err(CoordinatorError::Service)?;
                match self.service.state().map_err(CoordinatorError::Service)? {
                    ServiceState::Inactive => {}
                    state => return Err(CoordinatorError::UnexpectedKlipperState(state)),
                }
            }
            ServiceState::Inactive => {}
            state => return Err(CoordinatorError::UnexpectedKlipperState(state)),
        }

        self.builder
            .build(&approved.pending.prepared.request)
            .map_err(CoordinatorError::Build)
    }

    /// Builds an approved target, then invokes `flash` only after Klipper is stopped.
    pub fn execute_and_flash<E>(
        &self,
        approved: ApprovedBuild,
        flash: impl FnOnce(&PreparedBuild, &[u8]) -> Result<FlashResult, E>,
    ) -> Result<CompletedUpdate, FlashCoordinatorError<E>> {
        let mut prepared = approved.pending.prepared.clone();
        let artifact = self
            .execute(approved)
            .map_err(FlashCoordinatorError::Coordinator)?;
        prepared.request.kconfig = artifact.kconfig.clone();
        let firmware = std::fs::read(&artifact.path).map_err(FlashCoordinatorError::Artifact)?;
        let flash = flash(&prepared, &firmware).map_err(FlashCoordinatorError::Flash)?;
        Ok(CompletedUpdate { artifact, flash })
    }

    /// Builds an approved target and dispatches it through the native system backends.
    ///
    /// This preserves [`Self::execute_and_flash`]'s service boundary: Klipper is
    /// stopped before the build and is never restarted by this operation.
    pub fn execute_and_flash_system(
        &self,
        approved: ApprovedBuild,
        options: SystemFlashOptions,
    ) -> Result<CompletedUpdate, FlashCoordinatorError<SystemFlashError>> {
        self.execute_and_flash(approved, |prepared, firmware| {
            flash_prepared_system(prepared, firmware, options)
        })
    }
}
