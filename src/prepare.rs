use std::path::PathBuf;

use crate::build::BuildRequest;
use crate::moonraker::{McuInventory, McuTransport};

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

/// Errors while resolving a requested build target from Moonraker inventory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreparationError {
    /// The requested MCU is not present in the current Moonraker inventory.
    #[error("MCU target {0:?} is not present in Moonraker inventory")]
    TargetNotDiscovered(String),
}

/// Prepares a build request without writing files or invoking Klipper.
pub fn prepare_build(
    inventory: &McuInventory,
    target_name: &str,
    config_path: PathBuf,
    artifact_path: PathBuf,
    clean: bool,
) -> Result<PreparedBuild, PreparationError> {
    let discovered_mcu = inventory
        .mcus
        .iter()
        .find(|mcu| mcu.name == target_name)
        .ok_or_else(|| PreparationError::TargetNotDiscovered(target_name.to_owned()))?;

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
