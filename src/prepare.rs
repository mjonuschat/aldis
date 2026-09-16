use std::path::PathBuf;

use crate::build::BuildRequest;
use crate::moonraker::{McuInventory, McuTransport};
use crate::plan::UpdatePlan;

/// A selected MCU paired with the exact build request derived from Moonraker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedBuild {
    /// Moonraker's configured MCU object name.
    pub target_name: String,
    /// MCU identifier reported by the running firmware.
    pub mcu: String,
    /// The revalidated transport used by a later flashing backend.
    pub transport: Option<McuTransport>,
    /// Klipper build inputs ready for a later, explicit build operation.
    pub request: BuildRequest,
}

/// Errors while matching a requested build to the discovered update plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreparationError {
    /// The requested MCU is absent from the current update plan.
    #[error("MCU target {0:?} is not in the update plan")]
    TargetNotPlanned(String),
    /// The plan refers to an MCU no longer present in the inventory.
    #[error("MCU target {0:?} is no longer present in Moonraker inventory")]
    TargetNotDiscovered(String),
    /// The plan and live inventory disagree about the selected MCU type.
    #[error("MCU target {target_name:?} changed from {planned_mcu:?} to {discovered_mcu:?}")]
    McuMismatch {
        /// Moonraker's MCU object name.
        target_name: String,
        /// MCU identifier captured when the plan was made.
        planned_mcu: String,
        /// MCU identifier currently reported by Moonraker.
        discovered_mcu: String,
    },
    /// The plan and live inventory disagree about the selected transport.
    #[error(
        "MCU target {target_name:?} transport changed from {planned_transport:?} to {discovered_transport:?}"
    )]
    TransportMismatch {
        /// Moonraker's MCU object name.
        target_name: String,
        /// Transport captured when the plan was made.
        planned_transport: Option<McuTransport>,
        /// Transport currently reported by Moonraker.
        discovered_transport: Option<McuTransport>,
    },
}

/// Prepares a build request without writing files or invoking Klipper.
pub fn prepare_build(
    inventory: &McuInventory,
    plan: &UpdatePlan,
    target_name: &str,
    config_path: PathBuf,
    artifact_path: PathBuf,
    clean: bool,
) -> Result<PreparedBuild, PreparationError> {
    let planned_target = plan
        .targets
        .iter()
        .find(|target| target.name == target_name)
        .ok_or_else(|| PreparationError::TargetNotPlanned(target_name.to_owned()))?;
    let discovered_mcu = inventory
        .mcus
        .iter()
        .find(|mcu| mcu.name == target_name)
        .ok_or_else(|| PreparationError::TargetNotDiscovered(target_name.to_owned()))?;

    if planned_target.mcu != discovered_mcu.mcu {
        return Err(PreparationError::McuMismatch {
            target_name: target_name.to_owned(),
            planned_mcu: planned_target.mcu.clone(),
            discovered_mcu: discovered_mcu.mcu.clone(),
        });
    }
    if planned_target.transport != discovered_mcu.transport {
        return Err(PreparationError::TransportMismatch {
            target_name: target_name.to_owned(),
            planned_transport: planned_target.transport.clone(),
            discovered_transport: discovered_mcu.transport.clone(),
        });
    }

    Ok(PreparedBuild {
        target_name: target_name.to_owned(),
        mcu: discovered_mcu.mcu.clone(),
        transport: discovered_mcu.transport.clone(),
        request: BuildRequest {
            kconfig: discovered_mcu.kconfig.clone(),
            config_path,
            artifact_path,
            clean,
        },
    })
}
