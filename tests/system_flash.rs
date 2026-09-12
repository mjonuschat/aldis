use std::path::{Path, PathBuf};

use mcu_update::flash::system::{SerialFlashRoute, SystemFlashError, serial_route};
use mcu_update::flash::usb_bootloader::ObservedUsbBootloader;

fn observed(usb_id: &str) -> ObservedUsbBootloader {
    ObservedUsbBootloader {
        sysfs_path: PathBuf::from("/sys/devices/pci0000:00/usb1/1-2"),
        usb_id: usb_id.to_owned(),
        manufacturer: String::new(),
        serial_device: Some(PathBuf::from("/dev/ttyACM0")),
    }
}

#[test]
fn routes_each_supported_serial_bootloader_from_its_observed_identity() {
    assert!(matches!(
        serial_route(observed("1d50:6177"), ""),
        Ok(SerialFlashRoute::Katapult { serial_device }) if serial_device == Path::new("/dev/ttyACM0")
    ));
    assert!(matches!(
        serial_route(observed("0483:df11"), "CONFIG_STM32_FLASH_START_2000=y\n"),
        Ok(SerialFlashRoute::Stm32Dfu { sysfs_path, target })
            if sysfs_path == Path::new("/sys/devices/pci0000:00/usb1/1-2")
                && target.application_start == 0x0800_2000
    ));
    assert!(matches!(
        serial_route(observed("2e8a:0003"), ""),
        Ok(SerialFlashRoute::PicoBoot { sysfs_path })
            if sysfs_path == Path::new("/sys/devices/pci0000:00/usb1/1-2")
    ));
    assert!(matches!(
        serial_route(observed("2886:002f"), "CONFIG_SAMD_FLASH_START_2000=y\n"),
        Ok(SerialFlashRoute::Bossa { serial_device, target })
            if serial_device == Path::new("/dev/ttyACM0") && target.offset == 0x2000
    ));
}

#[test]
fn rejects_unknown_observed_bootloaders_before_selecting_a_route() {
    assert!(matches!(
        serial_route(observed("1234:5678"), "CONFIG_STM32_FLASH_START_2000=y\n"),
        Err(SystemFlashError::Selection(_))
    ));
}

#[test]
fn requires_a_valid_stm32_application_address_only_for_dfuse() {
    assert!(matches!(
        serial_route(observed("0483:df11"), ""),
        Err(SystemFlashError::Stm32Target(_))
    ));
    assert!(matches!(
        serial_route(observed("2e8a:0003"), ""),
        Ok(SerialFlashRoute::PicoBoot { .. })
    ));
    assert!(matches!(
        serial_route(observed("2886:002f"), ""),
        Err(SystemFlashError::BossaTarget(_))
    ));
}
