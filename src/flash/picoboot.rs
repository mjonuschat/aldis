//! Raspberry Pi PicoBoot firmware flashing.

use std::path::Path;
use std::time::Duration;

use picoboot::{Access, Picoboot};

use crate::flash::{FlashPort, FlashResult};

/// A decoded contiguous UF2 image suitable for PicoBoot flash commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uf2Image {
    /// Target address of the first firmware byte.
    pub address: u32,
    /// Contiguous firmware bytes.
    pub bytes: Vec<u8>,
}

/// UF2 decoding error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Uf2Error {
    /// The image does not consist of complete UF2 blocks.
    #[error("UF2 image length is not a multiple of the 512-byte block size")]
    InvalidLength,
    /// A UF2 block is malformed or not a main-flash RP2040 block.
    #[error("UF2 image contains a malformed or non-RP2040 block")]
    InvalidBlock,
    /// UF2 blocks do not form a contiguous image.
    #[error("UF2 image blocks do not form a contiguous address range")]
    NonContiguous,
}

/// PicoBoot transfer failure.
#[derive(Debug, thiserror::Error)]
pub enum PicoBootError {
    /// The Klipper artifact was not a supported UF2 image.
    #[error("firmware is not a supported UF2 image: {0}")]
    Uf2(#[source] Uf2Error),
    /// No RP2040 or RP2350 ROM bootloader is available.
    #[error("no RP2040 or RP2350 PicoBoot device found")]
    DeviceNotFound,
    /// More than one PicoBoot target is present, making selection unsafe.
    #[error("{0} PicoBoot devices found, expected exactly one")]
    AmbiguousDevice(usize),
    /// PicoBoot rejected a USB operation.
    #[error("PicoBoot transport error: {0}")]
    Transport(#[source] picoboot::Error),
    /// Flash readback differed from the supplied UF2 image.
    #[error("flash readback did not match the written UF2 image")]
    VerificationMismatch,
}

/// Decodes one contiguous RP2040 UF2 image.
pub fn decode_uf2(firmware: &[u8]) -> Result<Uf2Image, Uf2Error> {
    const BLOCK_SIZE: usize = 512;
    const MAGIC_START_0: u32 = 0x0a32_4655;
    const MAGIC_START_1: u32 = 0x9e5d_5157;
    const MAGIC_END: u32 = 0x0ab1_6f30;
    const FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;
    const RP2040_FAMILY_ID: u32 = 0xe48b_ff56;
    const PAGE_SIZE: usize = 256;
    const FLASH_START: u32 = 0x1000_0000;

    if firmware.is_empty() || !firmware.len().is_multiple_of(BLOCK_SIZE) {
        return Err(Uf2Error::InvalidLength);
    }

    let mut image = Vec::with_capacity(firmware.len() / 2);
    let mut start = None;
    let (blocks, []) = firmware.as_chunks::<BLOCK_SIZE>() else {
        return Err(Uf2Error::InvalidLength);
    };
    let block_count = blocks.len();
    for (index, block) in blocks.iter().enumerate() {
        let word = |offset| u32::from_le_bytes(block[offset..offset + 4].try_into().expect("word"));
        let address = word(12);
        let payload_size = word(16) as usize;
        if word(0) != MAGIC_START_0
            || word(4) != MAGIC_START_1
            || word(508) != MAGIC_END
            || word(8) & FLAG_FAMILY_ID_PRESENT == 0
            || word(28) != RP2040_FAMILY_ID
            || payload_size != PAGE_SIZE
            || word(20) != index as u32
            || word(24) != block_count as u32
            || address < FLASH_START
            || !address.is_multiple_of(PAGE_SIZE as u32)
        {
            return Err(Uf2Error::InvalidBlock);
        }
        if let Some(first_address) = start {
            if address != first_address + image.len() as u32 {
                return Err(Uf2Error::NonContiguous);
            }
        } else {
            start = Some(address);
        }
        image.extend_from_slice(&block[32..32 + payload_size]);
    }
    Ok(Uf2Image {
        address: start.expect("empty images are rejected"),
        bytes: image,
    })
}

/// Writes, reads back, and starts one Klipper UF2 image through PicoBoot.
pub fn flash_system(firmware: &[u8]) -> Result<FlashResult, PicoBootError> {
    flash_system_with_device(firmware, |_| true)
}

/// Writes, verifies, and starts one Klipper UF2 image at a selected USB topology.
pub fn flash_system_at_path(
    sysfs_path: &Path,
    firmware: &[u8],
) -> Result<FlashResult, PicoBootError> {
    flash_system_with_device(firmware, |device| device.sysfs_path() == sysfs_path)
}

/// A PicoBoot flash operation bound to one USB topology.
pub struct PicoBootAdapter<'a> {
    sysfs_path: &'a Path,
}

impl<'a> PicoBootAdapter<'a> {
    /// Binds a PicoBoot flash operation to its USB topology.
    pub fn new(sysfs_path: &'a Path) -> Self {
        Self { sysfs_path }
    }
}

impl FlashPort for PicoBootAdapter<'_> {
    type Error = PicoBootError;

    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error> {
        flash_system_at_path(self.sysfs_path, firmware)
    }
}

/// Reboots an already-detected PicoBoot device into its existing application,
/// without erasing or writing new firmware.
///
/// Unlike [`flash_system_at_path`], this assumes `sysfs_path` already is a
/// PicoBoot bootloader (as found by scanning the USB bus): whatever
/// application is already in flash is left untouched.
pub fn reboot_at_path(sysfs_path: &Path) -> Result<(), PicoBootError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let mut picoboot = find_device(|device| device.sysfs_path() == sysfs_path).await?;
            let connection = picoboot.connect().await.map_err(PicoBootError::Transport)?;
            connection
                .set_exclusive_access(Access::ExclusiveAndEject)
                .await
                .map_err(PicoBootError::Transport)?;
            connection
                .reboot(Duration::from_millis(500))
                .await
                .map_err(PicoBootError::Transport)?;
            Ok(())
        })
}

async fn find_device(
    matches: impl Fn(&nusb::DeviceInfo) -> bool,
) -> Result<Picoboot, PicoBootError> {
    let mut devices = Picoboot::list_devices(None)
        .await
        .map_err(PicoBootError::Transport)?
        .into_iter()
        .filter(matches)
        .collect::<Vec<_>>();
    let device = match devices.len() {
        0 => return Err(PicoBootError::DeviceNotFound),
        1 => devices.remove(0),
        count => return Err(PicoBootError::AmbiguousDevice(count)),
    };
    Picoboot::new(device)
        .await
        .map_err(PicoBootError::Transport)
}

fn flash_system_with_device(
    firmware: &[u8],
    matches: impl Fn(&nusb::DeviceInfo) -> bool,
) -> Result<FlashResult, PicoBootError> {
    let image = decode_uf2(firmware).map_err(PicoBootError::Uf2)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("Tokio runtime should initialize")
        .block_on(async {
            let mut picoboot = find_device(matches).await?;
            let connection = picoboot.connect().await.map_err(PicoBootError::Transport)?;
            connection
                .set_exclusive_access(Access::ExclusiveAndEject)
                .await
                .map_err(PicoBootError::Transport)?;
            connection
                .exit_xip()
                .await
                .map_err(PicoBootError::Transport)?;
            connection
                .flash_erase(
                    image.address,
                    (image.bytes.len() as u32).div_ceil(picoboot::SECTOR_SIZE)
                        * picoboot::SECTOR_SIZE,
                )
                .await
                .map_err(PicoBootError::Transport)?;
            connection
                .flash_write(image.address, &image.bytes)
                .await
                .map_err(PicoBootError::Transport)?;
            let readback = connection
                .flash_read(image.address, image.bytes.len() as u32)
                .await
                .map_err(PicoBootError::Transport)?;
            if readback != image.bytes {
                return Err(PicoBootError::VerificationMismatch);
            }
            connection
                .reboot(Duration::from_millis(500))
                .await
                .map_err(PicoBootError::Transport)?;
            Ok(FlashResult {
                reported_pages: None,
                padded_bytes: image.bytes.len(),
            })
        })
}
