use std::path::PathBuf;

use crate::build::BuildRequest;
use crate::kconfig::{self, InvalidKconfig};
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
    /// The MCU's reported Kconfig contains something other than Kconfig settings, which `make`
    /// would evaluate on the host; flash it by hand to bring it back under aldis.
    #[error("refusing to build {target:?} from the Kconfig its firmware reports")]
    UntrustedKconfig {
        /// Moonraker's configured MCU object name.
        target: String,
        /// The offending line.
        #[source]
        source: InvalidKconfig,
    },
    /// The MCU is an INDX, and its firmware is not built to run where the INDX bootloader places it.
    #[error("refusing to build {target:?} for the INDX bootloader")]
    IndxPlacement {
        /// Moonraker's configured MCU object name.
        target: String,
        /// What is wrong with the configuration.
        #[source]
        source: crate::flash::indx_dfu::TargetError,
    },
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
    // Refuse here, before Klipper is stopped for the build, as well as in the builder itself.
    kconfig::validate(&discovered_mcu.kconfig).map_err(|source| {
        PreparationError::UntrustedKconfig {
            target: target_name.to_owned(),
            source,
        }
    })?;

    if crate::flash::indx_dfu::is_indx_build(&discovered_mcu.kconfig) {
        crate::flash::indx_dfu::validate_kconfig(&discovered_mcu.kconfig).map_err(|source| {
            PreparationError::IndxPlacement {
                target: target_name.to_owned(),
                source,
            }
        })?;
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
