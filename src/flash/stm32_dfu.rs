//! STM32 USB-DFU backend boundaries backed by `dfu-nusb`.

use dfu_core::DfuIo;
use dfu_nusb::DfuNusb;
use nusb::MaybeFuture;

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

/// Derives the STM32 application address from Klipper's embedded Kconfig.
pub fn target_from_kconfig(
    kconfig: &str,
    erase_page_size: usize,
) -> Result<Stm32DfuTarget, Stm32DfuError> {
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
        erase_page_size,
    })
}

/// A native STM32 DFU transfer failure.
#[derive(Debug)]
pub enum Stm32DfuError {
    /// Embedded Kconfig did not select exactly one STM32 flash-start symbol.
    InvalidFlashStartConfiguration,
    /// DFU enumeration or device opening failed.
    Discovery(nusb::Error),
    /// No DFU device matched the explicitly configured USB identity.
    DeviceNotFound(Stm32DfuDevice),
    /// More than one DFU device matched the configured USB identity.
    AmbiguousDevice {
        device: Stm32DfuDevice,
        matches: usize,
    },
    /// The DFU transport rejected an operation.
    Transport(dfu_nusb::Error),
    /// Uploaded bytes did not match the firmware artifact.
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
    let dfu = dfu
        .manifest_without_wait()
        .map_err(Stm32DfuError::Transport)?;
    dfu.usb_reset().map_err(Stm32DfuError::Transport)?;
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
