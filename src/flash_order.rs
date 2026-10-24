//! The order in which selected MCUs are flashed.

use crate::moonraker::{Mcu, McuInventory};

/// Whether this MCU is a USB-to-CAN bridge; flashing one resets its CAN interface, cutting off
/// every CAN MCU behind it.
pub fn is_usb_can_bridge(mcu: &Mcu) -> bool {
    kconfig_selects_usb_can_bridge(&mcu.kconfig)
}

/// Whether an embedded Kconfig selects a USB-to-CAN bridge build.
///
/// The embedded configuration is a defconfig, so it names the architecture's own choice (for
/// example `RPXXXX_USBCANBUS`) and never the generic `USBCANBUS` that choice selects.
pub fn kconfig_selects_usb_can_bridge(kconfig: &str) -> bool {
    kconfig.lines().any(|line| {
        line.trim()
            .strip_prefix("CONFIG_")
            .and_then(|symbol| symbol.strip_suffix("=y"))
            .is_some_and(|symbol| symbol.contains("USBCANBUS") && !symbol.starts_with("HAVE_"))
    })
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
