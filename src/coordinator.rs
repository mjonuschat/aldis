use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use crate::build::{BuildArtifact, BuildError, CommandRunner, KlipperBuilder};
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
}
