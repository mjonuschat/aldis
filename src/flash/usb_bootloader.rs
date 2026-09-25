//! USB bootloader identities observed after a topology-scoped serial reset.

use std::path::{Path, PathBuf};

use crate::flash::usb_sysfs;

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

/// Diagnostic identity of a USB bootloader no supported backend recognizes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedBootloaderIdentity {
    /// USB vendor and product identifiers observed after re-enumeration.
    pub usb_id: String,
    /// USB manufacturer observed after re-enumeration.
    pub manufacturer: String,
    /// USB product string, or `"unknown"` when the device reported none.
    pub product: String,
    /// USB serial number string, or `"none"` when the device reported none.
    pub serial: String,
    /// USB device class byte, as reported (e.g. `"ef"`).
    pub device_class: String,
    /// USB device subclass byte, as reported.
    pub device_subclass: String,
    /// USB device protocol byte, as reported.
    pub device_protocol: String,
    /// Linux sysfs path identifying the physical USB topology observed.
    pub sysfs_path: String,
}

/// An observed USB bootloader cannot be used by a supported native backend.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UsbBootloaderSelectionError {
    /// No supported backend recognizes the observed bootloader identity.
    #[error(
        "USB device {} ({}) is not a supported bootloader\n  \
         product:      {}\n  \
         serial:       {}\n  \
         device class: {}/{}/{}\n  \
         sysfs path:   {}",
        .0.usb_id, .0.manufacturer, .0.product, .0.serial,
        .0.device_class, .0.device_subclass, .0.device_protocol, .0.sysfs_path
    )]
    Unsupported(Box<UnsupportedBootloaderIdentity>),
    /// Katapult appeared without a unique serial device at its topology.
    #[error("Katapult bootloader at {} has no unique serial device", .0.display())]
    KatapultSerialDeviceMissing(PathBuf),
}

/// Selects a native backend only from one observed USB bootloader identity.
pub fn select_usb_bootloader(
    observed: ObservedUsbBootloader,
) -> Result<SelectedUsbBootloader, UsbBootloaderSelectionError> {
    let sysfs_path = observed.sysfs_path.clone();
    let result = match observed.kind() {
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
        None => {
            let snapshot = usb_sysfs::usb_topology_snapshot(&sysfs_path);
            Err(UsbBootloaderSelectionError::Unsupported(Box::new(
                UnsupportedBootloaderIdentity {
                    usb_id: observed.usb_id,
                    manufacturer: observed.manufacturer,
                    product: non_empty_or(&snapshot.product, "unknown"),
                    serial: non_empty_or(&snapshot.serial, "none"),
                    device_class: snapshot.device_class,
                    device_subclass: snapshot.device_subclass,
                    device_protocol: snapshot.device_protocol,
                    sysfs_path: sysfs_path.display().to_string(),
                },
            )))
        }
    };

    match &result {
        Ok(selected) => tracing::debug!(selected = ?selected, "usb bootloader selected"),
        Err(error) => tracing::debug!(%error, "usb bootloader identity unsupported"),
    }

    result
}

fn non_empty_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

/// Scans every USB device directly under `root` and returns the ones whose
/// identity classifies as a known bootloader.
///
/// Unlike [`select_usb_bootloader`], this doesn't require a prior identity to
/// have changed away from: it inspects whatever is present right now, so it
/// can find a device that was already sitting in a bootloader before this
/// process started (e.g. after a failed update left it there).
pub fn scan_usb_bootloaders(root: &Path) -> Vec<ObservedUsbBootloader> {
    usb_sysfs::usb_device_dirs(root)
        .into_iter()
        .filter_map(|sysfs_path| {
            let identity = usb_sysfs::usb_identity(&sysfs_path);
            classify_usb_identity(&identity.usb_id, &identity.manufacturer)?;
            Some(ObservedUsbBootloader {
                serial_device: usb_sysfs::usb_tty(&sysfs_path),
                sysfs_path,
                usb_id: identity.usb_id,
                manufacturer: identity.manufacturer,
            })
        })
        .collect()
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
