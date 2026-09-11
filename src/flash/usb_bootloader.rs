//! USB bootloader identities observed after a topology-scoped serial reset.

use std::path::PathBuf;

const KATAPULT_USB_ID: &str = "1d50:6177";
const STM32_DFU_USB_ID: &str = "0483:df11";
const PICOBOOT_USB_IDS: &[&str] = &["2e8a:0003", "2e8a:000f"];

/// A USB bootloader supported by the native flashing backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbBootloaderKind {
    /// Katapult's USB serial bootloader.
    Katapult,
    /// STM32's built-in USB DFU bootloader.
    Stm32Dfu,
    /// Raspberry Pi's RP2040 or RP2350 PicoBoot ROM bootloader.
    PicoBoot,
}

/// A bootloader observed at the configured MCU's re-enumerated USB topology.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedUsbBootloader {
    /// Linux sysfs path identifying the physical USB topology.
    pub sysfs_path: PathBuf,
    /// Lowercase USB vendor and product identifiers separated by a colon.
    pub usb_id: String,
    /// Lowercase USB manufacturer string, when supplied by the device.
    pub manufacturer: String,
    /// USB serial device exposed by the bootloader, when it has exactly one.
    pub serial_device: Option<PathBuf>,
}

impl ObservedUsbBootloader {
    /// Returns the supported native backend matching this observed identity.
    pub fn kind(&self) -> Option<UsbBootloaderKind> {
        classify_usb_identity(&self.usb_id, &self.manufacturer)
    }
}

/// A supported USB bootloader bound to the topology that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectedUsbBootloader {
    /// Katapult's serial bootloader device and USB topology.
    Katapult {
        /// Linux sysfs path identifying the physical USB topology.
        sysfs_path: PathBuf,
        /// Katapult's serial device at that topology.
        serial_device: PathBuf,
    },
    /// STM32 ROM DFU at one USB topology.
    Stm32Dfu {
        /// Linux sysfs path identifying the physical USB topology.
        sysfs_path: PathBuf,
    },
    /// PicoBoot ROM bootloader at one USB topology.
    PicoBoot {
        /// Linux sysfs path identifying the physical USB topology.
        sysfs_path: PathBuf,
    },
}

/// An observed USB bootloader cannot be used by a supported native backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsbBootloaderSelectionError {
    /// No supported backend recognizes the observed bootloader identity.
    Unsupported {
        /// USB vendor and product identifiers observed after re-enumeration.
        usb_id: String,
        /// USB manufacturer observed after re-enumeration.
        manufacturer: String,
    },
    /// Katapult appeared without a unique serial device at its topology.
    KatapultSerialDeviceMissing(PathBuf),
}

/// Selects a native backend only from one observed USB bootloader identity.
pub fn select_usb_bootloader(
    observed: ObservedUsbBootloader,
) -> Result<SelectedUsbBootloader, UsbBootloaderSelectionError> {
    match observed.kind() {
        Some(UsbBootloaderKind::Katapult) => {
            let sysfs_path = observed.sysfs_path;
            let serial_device = observed.serial_device.ok_or_else(|| {
                UsbBootloaderSelectionError::KatapultSerialDeviceMissing(sysfs_path.clone())
            })?;
            Ok(SelectedUsbBootloader::Katapult {
                sysfs_path,
                serial_device,
            })
        }
        Some(UsbBootloaderKind::Stm32Dfu) => Ok(SelectedUsbBootloader::Stm32Dfu {
            sysfs_path: observed.sysfs_path,
        }),
        Some(UsbBootloaderKind::PicoBoot) => Ok(SelectedUsbBootloader::PicoBoot {
            sysfs_path: observed.sysfs_path,
        }),
        None => Err(UsbBootloaderSelectionError::Unsupported {
            usb_id: observed.usb_id,
            manufacturer: observed.manufacturer,
        }),
    }
}

/// Classifies one USB bootloader identity without inferring from Kconfig.
pub fn classify_usb_identity(usb_id: &str, manufacturer: &str) -> Option<UsbBootloaderKind> {
    if usb_id.eq_ignore_ascii_case(KATAPULT_USB_ID) || manufacturer.eq_ignore_ascii_case("katapult")
    {
        Some(UsbBootloaderKind::Katapult)
    } else if usb_id.eq_ignore_ascii_case(STM32_DFU_USB_ID) {
        Some(UsbBootloaderKind::Stm32Dfu)
    } else if PICOBOOT_USB_IDS
        .iter()
        .any(|id| usb_id.eq_ignore_ascii_case(id))
    {
        Some(UsbBootloaderKind::PicoBoot)
    } else {
        None
    }
}
