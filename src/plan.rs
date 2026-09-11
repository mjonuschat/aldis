use crate::moonraker::{McuInventory, McuTransport};

/// A read-only description of the update work required for discovered MCUs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    /// The service lifecycle shared by all targets in this run.
    pub klipper: KlipperLifecycle,
    /// MCU targets in the order they must be processed.
    pub targets: Vec<PlannedMcu>,
}

/// The Klipper state transitions an update command must make explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KlipperLifecycle {
    /// Discovery reads Moonraker while Klipper is still running.
    pub runs_during_discovery: bool,
    /// Klipper stops immediately before the first selected build or flash.
    pub stops_before_first_build_or_flash: bool,
    /// The operator must explicitly decide whether to start Klipper after a run.
    pub restarts_automatically: bool,
}

/// A single MCU's ordered update workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMcu {
    /// Moonraker's configured MCU object name.
    pub name: String,
    /// The MCU identifier reported by the running firmware.
    pub mcu: String,
    /// The configured host transport retained from read-only discovery.
    pub transport: Option<McuTransport>,
    /// The ordered actions that would be taken for this MCU.
    pub steps: Vec<UpdateStep>,
}

/// One explicit stage of a single-MCU update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStep {
    /// Validate the embedded Kconfig before producing firmware.
    ValidateConfiguration,
    /// Require confirmation immediately before a mutating operation.
    ConfirmWrite,
    /// Build using Klipper's existing pipeline.
    BuildFirmware,
    /// Ask the selected transport to enter its bootloader.
    EnterBootloader,
    /// Transfer the built artifact.
    FlashFirmware,
    /// Verify the transfer using the backend protocol.
    VerifyFirmware,
    /// Complete the bootloader session and run the application.
    CompleteBootloader,
}

impl UpdateStep {
    /// Returns a stable, human-readable phase label for terminal output.
    pub fn label(self) -> &'static str {
        match self {
            Self::ValidateConfiguration => "validate configuration",
            Self::ConfirmWrite => "confirm write",
            Self::BuildFirmware => "build firmware",
            Self::EnterBootloader => "enter bootloader",
            Self::FlashFirmware => "flash firmware",
            Self::VerifyFirmware => "verify firmware",
            Self::CompleteBootloader => "complete bootloader",
        }
    }
}

/// Builds a deterministic, non-mutating update plan from Moonraker inventory.
pub fn build_update_plan(inventory: &McuInventory) -> UpdatePlan {
    let steps = vec![
        UpdateStep::ValidateConfiguration,
        UpdateStep::ConfirmWrite,
        UpdateStep::BuildFirmware,
        UpdateStep::EnterBootloader,
        UpdateStep::FlashFirmware,
        UpdateStep::VerifyFirmware,
        UpdateStep::CompleteBootloader,
    ];

    UpdatePlan {
        klipper: KlipperLifecycle {
            runs_during_discovery: true,
            stops_before_first_build_or_flash: true,
            restarts_automatically: false,
        },
        targets: inventory
            .mcus
            .iter()
            .map(|mcu| PlannedMcu {
                name: mcu.name.clone(),
                mcu: mcu.mcu.clone(),
                transport: mcu.transport.clone(),
                steps: steps.clone(),
            })
            .collect(),
    }
}
