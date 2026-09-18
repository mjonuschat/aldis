//! STM32 USB-DFU backend boundaries backed by `dfu-nusb`.

use std::path::Path;

use dfu_nusb::{DfuNusb, DfuSync};
use nusb::MaybeFuture;
use nusb::transfer::TransferError;

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

/// Flash-start offsets (from `0x0800_0000`) Klipper's STM32 Kconfig can select.
const FLASH_START_OFFSETS_HEX: &[&str] = &[
    "800", "1000", "2000", "4000", "5000", "7000", "8000", "8800", "9000", "C000", "10000",
    "20000", "20200", "0000",
];

/// Derives the STM32 application address from Klipper's embedded Kconfig.
pub fn target_from_kconfig(kconfig: &str) -> Result<Stm32DfuTarget, Stm32DfuError> {
    let selected: Vec<_> = FLASH_START_OFFSETS_HEX
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

/// The outcome of probing a bootloader's flash for an existing application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationProbeResult {
    /// Exactly one candidate offset holds a plausible vector table.
    Found(u32),
    /// No candidate offset holds a plausible vector table.
    NotFound,
    /// More than one candidate offset looks plausible; selection is unsafe.
    Ambiguous(Vec<u32>),
}

/// Lowest address `Self::start`-inclusive STM32 SRAM can plausibly begin at.
const SRAM_START: u32 = 0x2000_0000;
/// An upper bound generous enough to cover every STM32 SRAM Klipper targets.
const SRAM_END: u32 = 0x2020_0000;
/// Lowest address STM32 internal flash begins at.
const FLASH_START: u32 = 0x0800_0000;
/// An upper bound generous enough to cover every STM32 flash Klipper targets.
const FLASH_END: u32 = 0x0820_0000;

/// Looks for an existing application by reading each candidate flash-start
/// offset's would-be vector table (initial stack pointer, reset handler) and
/// checking whether it looks plausible, without needing the Kconfig that
/// normally supplies this address.
///
/// `read_vector_table` reads the 8 bytes at one absolute flash address, or
/// returns `None` if that offset could not be read (skipped, not treated as
/// implausible). Only ever reports [`ApplicationProbeResult::Found`] when
/// exactly one candidate looks plausible.
pub fn probe_application_start(
    mut read_vector_table: impl FnMut(u32) -> Option<[u8; 8]>,
) -> ApplicationProbeResult {
    let matches: Vec<u32> = FLASH_START_OFFSETS_HEX
        .iter()
        .map(|offset| {
            FLASH_START
                + u32::from_str_radix(offset, 16).expect("FLASH_START_OFFSETS_HEX is valid hex")
        })
        .filter(|&address| {
            read_vector_table(address).is_some_and(|bytes| {
                let stack_pointer = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
                let reset_vector = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
                is_plausible_vector_table(stack_pointer, reset_vector)
            })
        })
        .collect();
    match matches.len() {
        0 => ApplicationProbeResult::NotFound,
        1 => ApplicationProbeResult::Found(matches[0]),
        _ => ApplicationProbeResult::Ambiguous(matches),
    }
}

/// Reads each candidate flash-start offset's would-be vector table from an
/// already-detected DFU device and applies [`probe_application_start`].
pub fn probe_application_start_at_path(
    identity: Stm32DfuDevice,
    sysfs_path: &Path,
) -> Result<ApplicationProbeResult, Stm32DfuError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let (device_info, interface_number) = find_device_at_path(identity, sysfs_path)?;
            let device = device_info.open().await.map_err(Stm32DfuError::Discovery)?;
            let interface = device
                .detach_and_claim_interface(interface_number)
                .await
                .map_err(Stm32DfuError::Discovery)?;
            let mut readings: Vec<(u32, [u8; 8])> = Vec::new();
            let mut session: Option<DfuSync> = None;
            for offset_hex in FLASH_START_OFFSETS_HEX {
                let offset = u32::from_str_radix(offset_hex, 16)
                    .expect("FLASH_START_OFFSETS_HEX is valid hex");
                let address = FLASH_START + offset;
                let dfu = match session.take() {
                    Some(dfu) => dfu,
                    None => match DfuNusb::open(device.clone(), interface.clone(), 0).await {
                        Ok(dfu) => dfu.into_sync_dfu(),
                        Err(error) => {
                            tracing::debug!(
                                address = %format_args!("{address:#x}"),
                                error = %Stm32DfuError::Transport(error),
                                "could not open a dfu session for candidate offset, skipping"
                            );
                            continue;
                        }
                    },
                };
                match dfu.upload_from_address(address, 8) {
                    Ok((next, bytes)) => {
                        session = Some(next);
                        if let Ok(bytes) = <[u8; 8]>::try_from(bytes.as_slice()) {
                            readings.push((address, bytes));
                        }
                    }
                    Err(error) => {
                        tracing::debug!(
                            address = %format_args!("{address:#x}"),
                            error = %Stm32DfuError::Transport(error),
                            "could not read candidate application offset, skipping"
                        );
                    }
                }
            }
            Ok(probe_application_start(|address| {
                readings
                    .iter()
                    .find(|(candidate, _)| *candidate == address)
                    .map(|(_, bytes)| *bytes)
            }))
        })
}

fn is_plausible_vector_table(stack_pointer: u32, reset_vector: u32) -> bool {
    (SRAM_START..SRAM_END).contains(&stack_pointer)
        && stack_pointer.is_multiple_of(4)
        && (FLASH_START..FLASH_END).contains(&reset_vector)
        && reset_vector % 2 == 1
}

/// Leaves DFU mode and starts the application at `application_start`,
/// without writing new firmware.
///
/// STM32's ROM DFU bootloader has no address-less "leave" command; this
/// mirrors `dfu-util`'s `<address>:leave` idiom by setting the address
/// pointer and sending a zero-length download to trigger manifestation.
pub async fn leave_at_address(
    device_info: nusb::DeviceInfo,
    interface_number: u8,
    application_start: u32,
) -> Result<(), Stm32DfuError> {
    let device = device_info.open().await.map_err(Stm32DfuError::Discovery)?;
    let interface = device
        .detach_and_claim_interface(interface_number)
        .await
        .map_err(Stm32DfuError::Discovery)?;
    let mut dfu = DfuNusb::open(device, interface, 0)
        .await
        .map_err(Stm32DfuError::Transport)?
        .into_sync_dfu();
    dfu.override_address(application_start);
    let dfu = dfu
        .download_without_manifest_from_slice(&[])
        .map_err(Stm32DfuError::Transport)?;
    match dfu.manifest_without_wait() {
        Ok(_) | Err(dfu_nusb::Error::Transfer(TransferError::Stall)) => Ok(()),
        Err(error) => Err(Stm32DfuError::Transport(error)),
    }
}

/// Discovers the selected STM32 DFU device at one USB topology and leaves it
/// at `application_start`, without writing new firmware.
pub fn leave_system_at_path(
    identity: Stm32DfuDevice,
    sysfs_path: &Path,
    application_start: u32,
) -> Result<(), Stm32DfuError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let (device, interface) = find_device_at_path(identity, sysfs_path)?;
            leave_at_address(device, interface, application_start).await
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
