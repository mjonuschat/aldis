//! STM32 USB-DFU backend boundaries backed by `dfu-nusb`.

use std::path::Path;
use std::time::Duration;

use dfu_nusb::DfuNusb;
use nusb::MaybeFuture;
use nusb::transfer::TransferError;

use crate::flash::katapult::serial::{SystemSerialIo, UsbBootloaderError};
use crate::flash::usb_bootloader::{
    SelectedUsbBootloader, UsbBootloaderSelectionError, select_usb_bootloader,
};
use crate::flash::{FlashPort, FlashResult};

/// An explicitly selected STM32 DFU USB identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stm32DfuDevice {
    /// USB vendor identifier.
    pub vendor_id: u16,
    /// USB product identifier.
    pub product_id: u16,
}

impl Stm32DfuDevice {
    /// STMicroelectronics' built-in ROM DFU bootloader identity.
    pub const ROM_BOOTLOADER: Self = Self {
        vendor_id: 0x0483,
        product_id: 0xdf11,
    };
}

/// STM32 flash placement required by the DFU backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stm32DfuTarget {
    /// Explicit application start address from the embedded build configuration.
    pub application_start: u32,
}

/// Derives the STM32 application address from Klipper's embedded Kconfig.
pub fn target_from_kconfig(kconfig: &str) -> Result<Stm32DfuTarget, Stm32DfuError> {
    let offsets = [
        "800", "1000", "2000", "4000", "5000", "7000", "8000", "8800", "9000", "C000", "10000",
        "20000", "20200", "0000",
    ];
    let selected: Vec<_> = offsets
        .iter()
        .filter(|offset| {
            kconfig
                .lines()
                .any(|line| line.trim() == format!("CONFIG_STM32_FLASH_START_{offset}=y"))
        })
        .collect();
    if selected.len() != 1 {
        return Err(Stm32DfuError::InvalidFlashStartConfiguration);
    }
    let offset = u32::from_str_radix(selected[0], 16)
        .map_err(|_| Stm32DfuError::InvalidFlashStartConfiguration)?;
    Ok(Stm32DfuTarget {
        application_start: 0x0800_0000 + offset,
    })
}

/// A native STM32 DFU transfer failure.
#[derive(Debug, thiserror::Error)]
pub enum Stm32DfuError {
    /// Embedded Kconfig did not select exactly one STM32 flash-start symbol.
    #[error("embedded Kconfig did not select exactly one STM32 flash-start symbol")]
    InvalidFlashStartConfiguration,
    /// DFU enumeration or device opening failed.
    #[error("STM32 DFU device discovery failed: {0}")]
    Discovery(#[source] nusb::Error),
    /// No DFU device matched the explicitly configured USB identity.
    #[error(
        "no STM32 DFU device found for vendor {:#06x} product {:#06x}",
        .0.vendor_id, .0.product_id
    )]
    DeviceNotFound(Stm32DfuDevice),
    /// More than one DFU device matched the configured USB identity.
    #[error(
        "{matches} STM32 DFU devices matched vendor {:#06x} product {:#06x}, expected exactly one",
        device.vendor_id, device.product_id
    )]
    AmbiguousDevice {
        device: Stm32DfuDevice,
        matches: usize,
    },
    /// The DFU transport rejected an operation.
    #[error("STM32 DFU transport error: {0}")]
    Transport(#[source] dfu_nusb::Error),
    /// Uploaded bytes did not match the firmware artifact.
    #[error("flash readback did not match the written firmware")]
    VerificationMismatch,
}

/// Failure while transitioning a running STM32 USB serial MCU to ROM DFU.
#[derive(Debug, thiserror::Error)]
pub enum Stm32DfuBootstrapError {
    /// The running USB serial MCU did not re-enumerate as STM32 ROM DFU.
    #[error(transparent)]
    Bootloader(UsbBootloaderError),
    /// The observed bootloader is unsupported or cannot be used safely.
    #[error(transparent)]
    Selection(UsbBootloaderSelectionError),
    /// A supported but non-STM32 bootloader appeared at the selected topology.
    #[error("expected STM32 ROM DFU but found {0:?}")]
    UnexpectedBootloader(SelectedUsbBootloader),
    /// Native DfuSe flashing failed after ROM DFU appeared.
    #[error(transparent)]
    Flash(Stm32DfuError),
}

/// Requests ROM DFU through a running USB serial STM32 MCU and flashes it.
pub fn bootstrap_system_serial(
    running_device: &Path,
    target: Stm32DfuTarget,
    firmware: &[u8],
    timeout: Duration,
    poll_interval: Duration,
) -> Result<FlashResult, Stm32DfuBootstrapError> {
    let bootloader = SystemSerialIo::request_and_observe_any_usb_bootloader(
        running_device,
        timeout,
        poll_interval,
    )
    .map_err(Stm32DfuBootstrapError::Bootloader)?;
    let bootloader_path =
        match select_usb_bootloader(bootloader).map_err(Stm32DfuBootstrapError::Selection)? {
            SelectedUsbBootloader::Stm32Dfu { sysfs_path } => sysfs_path,
            bootloader => return Err(Stm32DfuBootstrapError::UnexpectedBootloader(bootloader)),
        };
    flash_system_at_path(
        Stm32DfuDevice::ROM_BOOTLOADER,
        &bootloader_path,
        target,
        firmware,
    )
    .map_err(Stm32DfuBootstrapError::Flash)
}

/// Finds exactly one internal-flash DFU device matching `identity`.
pub fn find_device(identity: Stm32DfuDevice) -> Result<(nusb::DeviceInfo, u8), Stm32DfuError> {
    let matches = nusb::list_devices()
        .wait()
        .map_err(Stm32DfuError::Discovery)?
        .filter(|device| {
            device.vendor_id() == identity.vendor_id && device.product_id() == identity.product_id
        })
        .filter_map(|device| {
            let interface_number = device
                .interfaces()
                .find(|interface| interface.class() == 0xfe && interface.subclass() == 0x01)
                .map(|interface| interface.interface_number());
            interface_number.map(|interface_number| (device, interface_number))
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(Stm32DfuError::DeviceNotFound(identity)),
        1 => Ok(matches.into_iter().next().expect("length was checked")),
        count => Err(Stm32DfuError::AmbiguousDevice {
            device: identity,
            matches: count,
        }),
    }
}

/// Finds the internal-flash DFU device matching `identity` at `sysfs_path`.
///
/// The path is retained from a serial MCU's USB topology across bootloader
/// re-enumeration, so an unrelated ROM DFU device cannot be selected.
pub fn find_device_at_path(
    identity: Stm32DfuDevice,
    sysfs_path: &Path,
) -> Result<(nusb::DeviceInfo, u8), Stm32DfuError> {
    let matches = nusb::list_devices()
        .wait()
        .map_err(Stm32DfuError::Discovery)?
        .filter(|device| device.sysfs_path() == sysfs_path)
        .filter(|device| {
            device.vendor_id() == identity.vendor_id && device.product_id() == identity.product_id
        })
        .filter_map(|device| {
            let interface_number = device
                .interfaces()
                .find(|interface| interface.class() == 0xfe && interface.subclass() == 0x01)
                .map(|interface| interface.interface_number());
            interface_number.map(|interface_number| (device, interface_number))
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(Stm32DfuError::DeviceNotFound(identity)),
        1 => Ok(matches.into_iter().next().expect("length was checked")),
        count => Err(Stm32DfuError::AmbiguousDevice {
            device: identity,
            matches: count,
        }),
    }
}

/// Erases, writes, reads back, and manifests one STM32 application image.
pub async fn flash_device(
    device_info: nusb::DeviceInfo,
    interface_number: u8,
    target: Stm32DfuTarget,
    firmware: &[u8],
) -> Result<FlashResult, Stm32DfuError> {
    let device = device_info.open().await.map_err(Stm32DfuError::Discovery)?;
    let interface = device
        .detach_and_claim_interface(interface_number)
        .await
        .map_err(Stm32DfuError::Discovery)?;
    let mut dfu = DfuNusb::open(device, interface, 0)
        .await
        .map_err(Stm32DfuError::Transport)?
        .into_sync_dfu();
    dfu.override_address(target.application_start);
    let dfu = dfu
        .download_without_manifest_from_slice(firmware)
        .map_err(Stm32DfuError::Transport)?;
    let (dfu, readback) = dfu
        .upload_from_address(target.application_start, firmware.len())
        .map_err(Stm32DfuError::Transport)?;
    if readback != firmware {
        return Err(Stm32DfuError::VerificationMismatch);
    }
    match dfu.manifest_without_wait() {
        Ok(_) | Err(dfu_nusb::Error::Transfer(TransferError::Stall)) => {}
        Err(error) => return Err(Stm32DfuError::Transport(error)),
    }
    Ok(FlashResult {
        reported_pages: None,
        padded_bytes: firmware.len(),
    })
}

/// Discovers the selected STM32 DFU device and flashes it synchronously.
///
/// Call only after the coordinator has stopped Klipper and the operator has
/// explicitly approved the physical update.
pub fn flash_system(
    identity: Stm32DfuDevice,
    target: Stm32DfuTarget,
    firmware: &[u8],
) -> Result<FlashResult, Stm32DfuError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let (device, interface) = find_device(identity)?;
            flash_device(device, interface, target, firmware).await
        })
}

/// Discovers the selected STM32 DFU device at one USB topology and flashes it.
pub fn flash_system_at_path(
    identity: Stm32DfuDevice,
    sysfs_path: &Path,
    target: Stm32DfuTarget,
    firmware: &[u8],
) -> Result<FlashResult, Stm32DfuError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let (device, interface) = find_device_at_path(identity, sysfs_path)?;
            flash_device(device, interface, target, firmware).await
        })
}

/// An STM32 DFU flash operation bound to one USB topology and application target.
pub struct Stm32DfuAdapter<'a> {
    identity: Stm32DfuDevice,
    sysfs_path: &'a Path,
    target: Stm32DfuTarget,
}

impl<'a> Stm32DfuAdapter<'a> {
    /// Binds an STM32 DFU flash operation to its device identity, USB topology, and target.
    pub fn new(identity: Stm32DfuDevice, sysfs_path: &'a Path, target: Stm32DfuTarget) -> Self {
        Self {
            identity,
            sysfs_path,
            target,
        }
    }
}

impl FlashPort for Stm32DfuAdapter<'_> {
    type Error = Stm32DfuError;

    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error> {
        flash_system_at_path(self.identity, self.sysfs_path, self.target, firmware)
    }
}
