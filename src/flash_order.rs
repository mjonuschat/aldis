//! The order in which selected MCUs are flashed.

use crate::moonraker::{Mcu, McuInventory};

/// Whether this MCU is a USB-to-CAN bridge; flashing one resets its CAN interface, cutting off
/// every CAN MCU behind it.
pub fn is_usb_can_bridge(mcu: &Mcu) -> bool {
    mcu.kconfig
        .lines()
        .any(|line| line.trim() == "CONFIG_USBCANBUS=y")
}

/// Orders `targets` for flashing: bridges last, everything else in its given order.
pub fn flash_order(inventory: &McuInventory, targets: &[String]) -> Vec<String> {
    let is_bridge = |name: &String| {
        inventory
            .mcus
            .iter()
            .find(|mcu| &mcu.name == name)
            .is_some_and(is_usb_can_bridge)
    };
    let (bridges, others): (Vec<_>, Vec<_>) = targets.iter().cloned().partition(is_bridge);
    others.into_iter().chain(bridges).collect()
}
