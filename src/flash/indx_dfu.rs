//! Bondtech INDX toolboard: a SAME51 whose own USB DFU bootloader (`indx-bootloader`) takes the
//! Klipper image over a plain DFU download.
//!
//! The bootloader writes what it receives at its application offset, so no address is sent, and
//! it never reads the flash back (`DFU_UPLOAD` is answered with an empty reply). It starts the
//! application only when the image carries its own length and checksum:
//!
//! - the word at offset `0x1C` of the image is the address one past the end of the firmware,
//! - and a CRC-32 of everything before that address is stored there, as the last four bytes.
//!
//! An image that fails the check leaves the board in the bootloader, so a failed flash can be
//! repeated. Klipper's 1200-baud touch or the CAN RESET jumper enters the same bootloader.

use std::path::Path;
use std::time::Duration;

use flate2::Crc;

use crate::flash::FlashResult;
use crate::flash::plain_dfu::{self, PlainDfuError};
use crate::flash::usb_sysfs;
use crate::retry::retry_until_available;

/// USB vendor id of the bootloader and of the running firmware, which share both ids.
pub const VENDOR_ID: u16 = 0x04d8;
/// USB product id of the bootloader and of the running firmware.
pub const PRODUCT_ID: u16 = 0xe483;

/// Offset in the image of the address one past the end of the firmware.
const END_POINTER_OFFSET: usize = 0x1c;
/// The shortest image that holds that word.
const MIN_IMAGE_LENGTH: usize = END_POINTER_OFFSET + 4;

/// Whether `usb_id` is the one the INDX's bootloader and its application share.
pub fn has_board_ids(usb_id: &str) -> bool {
    usb_id.eq_ignore_ascii_case(&format!("{VENDOR_ID:04x}:{PRODUCT_ID:04x}"))
}

/// Whether a USB device with these ids is the INDX bootloader. The application has the same ids,
/// so what tells them apart is the bootloader's DFU interface.
pub fn is_bootloader(usb_id: &str, dfu_interface: bool) -> bool {
    has_board_ids(usb_id) && dfu_interface
}

/// Whether a USB device with these ids is a running INDX firmware, which has no DFU interface. The
/// check ignores the manufacturer and product strings, which are build options
/// (`CONFIG_USB_MANUFACTURER`, `CONFIG_USB_PRODUCT`).
pub fn is_application(usb_id: &str, dfu_interface: bool) -> bool {
    has_board_ids(usb_id) && !dfu_interface
}

/// Where the bootloader writes firmware: it places the first block at its own fixed offset, the
/// 16 KiB it occupies. Firmware linked for any other address would not run.
pub const APPLICATION_START: u32 = 0x4000;
/// Flash size of the SAME51J19A.
const FLASH_SIZE: u32 = 0x8_0000;

/// Why a Kconfig cannot be flashed to an INDX bootloader.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TargetError {
    /// The firmware is not an INDX build.
    #[error("the firmware is not built for the Bondtech INDX")]
    NotIndx,
    /// The firmware was built to start at flash address zero, where the bootloader lives.
    #[error("the firmware would overwrite the INDX's bootloader")]
    NoBootloaderOffset,
    /// The firmware is linked for an address the INDX bootloader does not place it at.
    #[error("the firmware is built for a different memory layout than the INDX's bootloader uses")]
    WrongApplicationAddress,
}

/// Whether a Kconfig is for the INDX toolboard's MCU and enables its heater driver. Klipper with
/// `indx_klipper` and Kalico both define `WANT_INDX_HEATER`; stock Klipper has no such symbol.
pub fn is_indx_build(kconfig: &str) -> bool {
    let enabled = |symbol: &str| kconfig.lines().any(|line| line.trim() == symbol);
    enabled("CONFIG_MACH_SAME51J19=y") && enabled("CONFIG_WANT_INDX_HEATER=y")
}

fn hex_value(kconfig: &str, symbol: &str) -> Option<u32> {
    kconfig
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(symbol)?.strip_prefix('='))
        .and_then(|value| u32::from_str_radix(value.strip_prefix("0x")?, 16).ok())
}

/// Checks that a Kconfig describes firmware the INDX bootloader can run. The embedded Kconfig is a
/// defconfig, which leaves out what is at its default, such as the 0x4000 application address.
pub fn validate_kconfig(kconfig: &str) -> Result<(), TargetError> {
    if !is_indx_build(kconfig) {
        return Err(TargetError::NotIndx);
    }
    if kconfig
        .lines()
        .any(|line| line.trim() == "CONFIG_SAMD_FLASH_START_0000=y")
    {
        return Err(TargetError::NoBootloaderOffset);
    }
    match hex_value(kconfig, "CONFIG_FLASH_APPLICATION_ADDRESS") {
        Some(address) if address != APPLICATION_START => {
            tracing::debug!(
                address,
                expected = APPLICATION_START,
                "INDX firmware start address"
            );
            Err(TargetError::WrongApplicationAddress)
        }
        _ => Ok(()),
    }
}

/// The SAME51J19A's RAM: where its firmware's initial stack pointer, the first vector, must lie.
/// The stack starts at the top, so the end is included.
const RAM: std::ops::RangeInclusive<u32> = 0x2000_0000..=0x2003_0000;

/// An image the bootloader would not start, or that does not fit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ImageError {
    /// Too short to hold the vector table word the bootloader reads the length from.
    #[error("the file is too small to be INDX firmware")]
    TooSmall,
    /// The first two vectors are not those of firmware for this MCU at this address. The bootloader
    /// starts any image whose length and checksum are right, so a prepared binary for another
    /// MCU would start and crash.
    #[error("the file is not firmware for the INDX")]
    NotIndxFirmware,
    /// The image and its checksum do not fit below the end of flash.
    #[error("the firmware is too large for the INDX")]
    TooLarge,
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .expect("a four-byte slice"),
    )
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    crc.sum()
}

/// The length of the firmware in `image` when it is already prepared: its end word points at the
/// checksum, which is the last four bytes.
fn prepared_firmware_length(image: &[u8]) -> Option<usize> {
    let length = read_u32(image, END_POINTER_OFFSET).checked_sub(APPLICATION_START)? as usize;
    (length + 4 == image.len() && length >= MIN_IMAGE_LENGTH).then_some(length)
}

/// Prepares `image`, linked for [`APPLICATION_START`], for the bootloader: points the word at
/// offset `0x1C` at the end of the firmware and stores the CRC-32 of the firmware there. An already
/// prepared image comes back unchanged. The bootloader refuses firmware that, with its checksum,
/// does not fit below the end of flash.
pub fn prepare_image(image: &[u8]) -> Result<Vec<u8>, ImageError> {
    if image.len() < MIN_IMAGE_LENGTH {
        return Err(ImageError::TooSmall);
    }

    let firmware_length = prepared_firmware_length(image).unwrap_or(image.len());
    let available = FLASH_SIZE - APPLICATION_START - 4;
    let length = u32::try_from(firmware_length)
        .ok()
        .filter(|length| *length <= available)
        .ok_or_else(|| {
            tracing::debug!(length = firmware_length, available, "INDX firmware size");
            ImageError::TooLarge
        })?;

    let end = APPLICATION_START + length;
    let mut firmware = image[..firmware_length].to_vec();
    firmware[END_POINTER_OFFSET..END_POINTER_OFFSET + 4].copy_from_slice(&end.to_le_bytes());

    // The first two words of a Cortex-M image are its initial stack pointer and reset vector.
    let (stack_pointer, reset_vector) = (read_u32(&firmware, 0), read_u32(&firmware, 4));
    if !RAM.contains(&stack_pointer)
        || reset_vector & 1 == 0
        || !(APPLICATION_START..end).contains(&(reset_vector & !1))
    {
        tracing::debug!(
            stack_pointer,
            reset_vector,
            start = APPLICATION_START,
            end,
            "INDX firmware vectors"
        );
        return Err(ImageError::NotIndxFirmware);
    }

    firmware.extend_from_slice(&crc32(&firmware).to_le_bytes());
    Ok(firmware)
}

/// Failure while flashing the INDX bootloader.
#[derive(Debug, thiserror::Error)]
pub enum IndxDfuError {
    /// USB enumeration or opening the bootloader failed.
    #[error("could not open the INDX bootloader: {0}")]
    Discovery(#[source] nusb::Error),
    /// The bootloader disappeared, or exposed no DFU interface.
    #[error("no INDX DFU bootloader was found at its USB topology")]
    DeviceNotFound,
    /// The DFU transfer failed.
    #[error("the INDX bootloader rejected or failed the firmware download: {0}")]
    Transport(#[source] dfu_nusb::Error),
}

/// Downloads a prepared `image` (see [`prepare_image`]) to the INDX bootloader at `sysfs_path` and
/// starts its manifestation, on which it resets into the new firmware. Nothing can be read back,
/// so the caller must confirm that the firmware runs: the board appears again with the
/// application's USB identity (see [`is_application`]).
pub fn flash(sysfs_path: &Path, image: &[u8]) -> Result<FlashResult, IndxDfuError> {
    plain_dfu::download(sysfs_path, VENDOR_ID, PRODUCT_ID, image).map_err(|error| match error {
        PlainDfuError::Discovery(error) => IndxDfuError::Discovery(error),
        PlainDfuError::DeviceNotFound => IndxDfuError::DeviceNotFound,
        PlainDfuError::Transport(error) => IndxDfuError::Transport(error),
    })?;
    Ok(FlashResult {
        reported_pages: None,
        padded_bytes: image.len(),
    })
}

/// Leaves the INDX bootloader at `sysfs_path` without a new image. A download that ends, as
/// every one does, with a zero-length block makes the bootloader reset. It then starts the
/// application, or stays in the bootloader if the checksum is wrong.
pub fn leave(sysfs_path: &Path) -> Result<(), IndxDfuError> {
    flash(sysfs_path, &[]).map(|_| ())
}

/// Whether the device at `usb_path` is a running INDX firmware, serial port included.
pub fn application_at(usb_path: &Path) -> bool {
    let identity = usb_sysfs::usb_identity(usb_path);
    is_application(&identity.usb_id, usb_sysfs::has_dfu_interface(usb_path))
        && usb_sysfs::usb_tty(usb_path).is_some()
}

/// Waits until the INDX at `usb_path` runs its application again. Returns whether it did before
/// `timeout`. A board whose image the bootloader rejected stays in the bootloader.
pub fn wait_for_application(usb_path: &Path, timeout: Duration, poll_interval: Duration) -> bool {
    retry_until_available(timeout, poll_interval, || {
        application_at(usb_path).then_some(()).ok_or(())
    })
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u32 = APPLICATION_START;
    const PREPARED: &[u8] = include_bytes!("../../tests/fixtures/indx_klipper.bin");

    fn word(image: &[u8]) -> u32 {
        u32::from_le_bytes(image[0x1c..0x20].try_into().unwrap())
    }

    #[test]
    fn recognizes_the_bootloader_and_not_the_application_with_the_same_ids() {
        assert!(is_bootloader("04d8:e483", true));
        assert!(is_bootloader("04D8:E483", true));
        assert!(!is_bootloader("04d8:e483", false));
        assert!(!is_bootloader("04d8:beac", true));

        assert!(is_application("04d8:e483", false));
        assert!(!is_application("04d8:e483", true));
        assert!(!is_application("04d8:beac", false));
    }

    const KALICO_DEFCONFIG: &str = "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_ATSAMD=y\n\
        CONFIG_MACH_SAME51J19=y\nCONFIG_CLOCK_REF_X25M_XOSC0=y\nCONFIG_USB_VENDOR_ID=0x04d8\n\
        CONFIG_USB_DEVICE_ID=0xe483\nCONFIG_WANT_INDX_HEATER=y\n";

    #[test]
    fn accepts_a_defconfig_and_an_expanded_config_at_the_bootloaders_address() {
        assert_eq!(validate_kconfig(KALICO_DEFCONFIG), Ok(()));
        let expanded = format!("{KALICO_DEFCONFIG}CONFIG_FLASH_APPLICATION_ADDRESS=0x4000\n");
        assert_eq!(validate_kconfig(&expanded), Ok(()));
    }

    #[test]
    fn refuses_firmware_linked_for_an_address_the_bootloader_does_not_use() {
        let elsewhere = format!("{KALICO_DEFCONFIG}CONFIG_FLASH_APPLICATION_ADDRESS=0x10000\n");
        assert_eq!(
            validate_kconfig(&elsewhere),
            Err(TargetError::WrongApplicationAddress)
        );
    }

    #[test]
    fn refuses_firmware_that_is_not_an_indx_build() {
        let same51 = "CONFIG_MACH_SAME51J19=y\nCONFIG_CLOCK_REF_X25M_XOSC0=y\n";
        let heater_elsewhere = "CONFIG_MACH_SAMD51J19=y\nCONFIG_WANT_INDX_HEATER=y\n";
        assert_eq!(validate_kconfig(same51), Err(TargetError::NotIndx));
        assert_eq!(
            validate_kconfig(heater_elsewhere),
            Err(TargetError::NotIndx)
        );
        assert!(!is_indx_build(""));
    }

    #[test]
    fn refuses_firmware_that_would_replace_the_bootloader() {
        let no_offset = format!("{KALICO_DEFCONFIG}CONFIG_SAMD_FLASH_START_0000=y\n");
        assert_eq!(
            validate_kconfig(&no_offset),
            Err(TargetError::NoBootloaderOffset)
        );
    }

    #[test]
    fn sees_the_application_only_once_its_serial_port_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let usb = &dir.path().join("1-1");
        let interface = |name: &str, class: &str, subclass: &str| {
            let path = usb.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("bInterfaceClass"), format!("{class}\n")).unwrap();
            std::fs::write(path.join("bInterfaceSubClass"), format!("{subclass}\n")).unwrap();
        };
        std::fs::create_dir_all(usb).unwrap();
        std::fs::write(usb.join("idVendor"), "04d8\n").unwrap();
        std::fs::write(usb.join("idProduct"), "e483\n").unwrap();
        interface("1-1:1.0", "fe", "01");
        assert!(
            !application_at(usb),
            "the bootloader is not the application"
        );

        std::fs::remove_dir_all(usb.join("1-1:1.0")).unwrap();
        interface("1-1:1.0", "02", "02");
        assert!(!application_at(usb), "the serial port has not appeared");

        std::fs::create_dir_all(usb.join("1-1:1.0").join("tty").join("ttyACM1")).unwrap();
        assert!(application_at(usb));
        assert!(wait_for_application(
            usb,
            Duration::from_millis(20),
            Duration::from_millis(5)
        ));
    }

    #[test]
    fn leaves_an_already_prepared_image_unchanged() {
        assert_eq!(prepare_image(PREPARED).unwrap(), PREPARED);
    }

    #[test]
    fn prepares_an_image_the_linker_left_without_a_checksum() {
        let firmware = &PREPARED[..PREPARED.len() - 4];
        let mut raw = firmware.to_vec();
        raw[0x1c..0x20].copy_from_slice(&0x9a05u32.to_le_bytes());

        let prepared = prepare_image(&raw).unwrap();

        assert_eq!(prepared, PREPARED);
        assert_eq!(word(&prepared), START + firmware.len() as u32);
        let (body, stored) = prepared.split_at(prepared.len() - 4);
        assert_eq!(stored, crc32(body).to_le_bytes());
    }

    #[test]
    fn accepts_an_image_whose_word_already_points_at_its_end() {
        let mut raw = PREPARED[..PREPARED.len() - 4].to_vec();
        let end = START + raw.len() as u32;
        raw[0x1c..0x20].copy_from_slice(&end.to_le_bytes());

        assert_eq!(prepare_image(&raw).unwrap(), PREPARED);
    }

    #[test]
    fn repairs_a_stale_checksum_in_place() {
        let mut stale = PREPARED.to_vec();
        let last = stale.len() - 1;
        stale[last] ^= 0xff;

        assert_eq!(prepare_image(&stale).unwrap(), PREPARED);
    }

    fn image(start: u32, length: usize) -> Vec<u8> {
        let mut image = vec![0u8; length];
        image[..4].copy_from_slice(&0x2003_0000u32.to_le_bytes());
        image[4..8].copy_from_slice(&(start + 0x101).to_le_bytes());
        image
    }

    #[test]
    fn refuses_an_image_that_would_run_past_the_end_of_flash() {
        let firmware = image(START, FLASH_SIZE as usize - START as usize - 4);
        assert!(prepare_image(&firmware).is_ok());

        let too_large = image(START, FLASH_SIZE as usize - START as usize - 3);
        assert_eq!(prepare_image(&too_large), Err(ImageError::TooLarge));
    }

    #[test]
    fn handles_an_image_that_is_exactly_the_length_word_without_panicking() {
        let mut short = image(START, 0x20);
        short[4..8].copy_from_slice(&(START + 1).to_le_bytes());
        short[0x1c..0x20].copy_from_slice(&(START + 0x1c).to_le_bytes());

        let prepared = prepare_image(&short).unwrap();

        assert_eq!(
            prepared.len(),
            0x24,
            "treated as unprepared firmware plus a checksum"
        );
        assert_eq!(word(&prepared), START + 0x20);
    }

    #[test]
    fn refuses_an_image_too_small_to_hold_the_length_word() {
        assert_eq!(prepare_image(&[0; 0x1f]), Err(ImageError::TooSmall));
    }

    #[test]
    fn refuses_a_binary_that_is_not_firmware_for_this_mcu_and_address() {
        let refused =
            |image: &[u8]| matches!(prepare_image(image), Err(ImageError::NotIndxFirmware));
        let with = |stack_pointer: u32, reset_vector: u32| {
            let mut firmware = image(START, 4096);
            firmware[..4].copy_from_slice(&stack_pointer.to_le_bytes());
            firmware[4..8].copy_from_slice(&reset_vector.to_le_bytes());
            firmware
        };

        assert!(!refused(&with(0x2003_0000, START + 0x101)));
        assert!(!refused(&with(0x2000_8000, START + 1)), "the first byte");
        assert!(refused(&[0u8; 4096]), "zeros are no firmware");
        assert!(refused(&[0xffu8; 4096]), "erased flash is no firmware");
        assert!(refused(&with(0x2004_0000, START + 0x101)), "above RAM");
        assert!(refused(&with(0x1fff_ff00, START + 0x101)), "below RAM");
        assert!(refused(&with(0x2003_0000, START + 0x100)), "not Thumb");
        assert!(
            refused(&with(0x2003_0000, 0x0000_1001)),
            "before the application"
        );
        assert!(
            refused(&with(0x2003_0000, START + 0x1001)),
            "past the image"
        );
        assert!(
            refused(&with(0x2002_0000, 0x0800_0201)),
            "another MCU's flash"
        );
    }

    #[test]
    fn checks_the_vectors_of_an_already_prepared_image_too() {
        let mut prepared = PREPARED.to_vec();
        prepared[4..8].copy_from_slice(&0x0800_0201u32.to_le_bytes());

        assert!(matches!(
            prepare_image(&prepared),
            Err(ImageError::NotIndxFirmware)
        ));
    }
}
