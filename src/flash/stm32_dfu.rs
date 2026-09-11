//! STM32 USB-DFU backend boundaries backed by `dfu-rs`.

use dfu_rs::{DEFAULT_USB_TIMEOUT, Device, DfuType, search_for_dfu};
use futures::executor::block_on;

use crate::flash::FlashResult;

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
    /// The explicit erase-page size for this MCU family.
    pub erase_page_size: usize,
}

/// A native STM32 DFU transfer failure.
#[derive(Debug)]
pub enum Stm32DfuError {
    /// DFU enumeration failed.
    Discovery(dfu_rs::Error),
    /// No DFU device matched the explicitly configured USB identity.
    DeviceNotFound(Stm32DfuDevice),
    /// More than one DFU device matched the configured USB identity.
    AmbiguousDevice {
        device: Stm32DfuDevice,
        matches: usize,
    },
    /// The configured erase geometry is invalid.
    InvalidErasePageSize,
    /// The DFU transport rejected an operation.
    Transport(dfu_rs::Error),
    /// Uploaded bytes did not match the firmware artifact.
    VerificationMismatch,
}

/// Finds exactly one internal-flash DFU device matching `identity`.
pub async fn find_device(identity: Stm32DfuDevice) -> Result<Device, Stm32DfuError> {
    let matches = search_for_dfu(DEFAULT_USB_TIMEOUT, Some(DfuType::InternalFlash))
        .await
        .map_err(Stm32DfuError::Discovery)?
        .into_iter()
        .filter(|device| {
            let info = device.info();
            info.vid() == identity.vendor_id && info.pid() == identity.product_id
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

/// Erases, writes, and reads back one STM32 application image.
pub async fn flash_device(
    device: &Device,
    target: Stm32DfuTarget,
    firmware: &[u8],
) -> Result<FlashResult, Stm32DfuError> {
    if target.erase_page_size == 0 {
        return Err(Stm32DfuError::InvalidErasePageSize);
    }
    device
        .erase(
            target.application_start,
            firmware.len(),
            target.erase_page_size,
        )
        .await
        .map_err(Stm32DfuError::Transport)?;
    device
        .download(target.application_start, firmware)
        .await
        .map_err(Stm32DfuError::Transport)?;
    let readback = device
        .upload(target.application_start, firmware.len())
        .await
        .map_err(Stm32DfuError::Transport)?;
    if readback != firmware {
        return Err(Stm32DfuError::VerificationMismatch);
    }
    Ok(FlashResult {
        pages_written: firmware.len().div_ceil(target.erase_page_size) as u32,
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
    block_on(async {
        let device = find_device(identity).await?;
        flash_device(&device, target, firmware).await
    })
}
