use std::path::PathBuf;

use mcu_update::flash::usb_bootloader::{
    ObservedUsbBootloader, SelectedUsbBootloader, UsbBootloaderKind, UsbBootloaderSelectionError,
    classify_usb_identity, select_usb_bootloader,
};

#[test]
fn classifies_supported_usb_bootloaders_case_insensitively() {
    assert_eq!(
        classify_usb_identity("1D50:6177", "unknown"),
        Some(UsbBootloaderKind::Katapult)
    );
    assert_eq!(
        classify_usb_identity("other", "KATAPULT"),
        Some(UsbBootloaderKind::Katapult)
    );
    assert_eq!(
        classify_usb_identity("0483:DF11", "STMicroelectronics"),
        Some(UsbBootloaderKind::Stm32Dfu)
    );
    assert_eq!(
        classify_usb_identity("2E8A:0003", "Raspberry Pi"),
        Some(UsbBootloaderKind::PicoBoot)
    );
    assert_eq!(
        classify_usb_identity("2e8a:000f", "Raspberry Pi"),
        Some(UsbBootloaderKind::PicoBoot)
    );
}

#[test]
fn leaves_unknown_usb_bootloaders_unsupported() {
    assert_eq!(
        classify_usb_identity("0483:5740", "STMicroelectronics"),
        None
    );
}

#[test]
fn selects_supported_bootloaders_only_at_the_observed_topology() {
    let sysfs_path = PathBuf::from("/sys/devices/platform/usb/1-1.2");
    let observed = |usb_id: &str| ObservedUsbBootloader {
        sysfs_path: sysfs_path.clone(),
        usb_id: usb_id.to_owned(),
        manufacturer: String::new(),
        serial_device: Some(PathBuf::from("/dev/ttyACM0")),
    };

    assert!(matches!(
        select_usb_bootloader(observed("1d50:6177")),
        Ok(SelectedUsbBootloader::Katapult { sysfs_path: path, .. }) if path == sysfs_path
    ));
    assert!(matches!(
        select_usb_bootloader(observed("0483:df11")),
        Ok(SelectedUsbBootloader::Stm32Dfu { sysfs_path: path }) if path == sysfs_path
    ));
    assert!(matches!(
        select_usb_bootloader(observed("2e8a:0003")),
        Ok(SelectedUsbBootloader::PicoBoot { sysfs_path: path }) if path == sysfs_path
    ));
}

#[test]
fn rejects_unknown_bootloaders_before_a_backend_can_flash() {
    let observed = ObservedUsbBootloader {
        sysfs_path: PathBuf::from("/sys/devices/platform/usb/1-1.3"),
        usb_id: "1209:0001".to_owned(),
        manufacturer: "example".to_owned(),
        serial_device: None,
    };

    assert!(matches!(
        select_usb_bootloader(observed),
        Err(UsbBootloaderSelectionError::Unsupported { .. })
    ));
}

#[test]
fn rejects_katapult_without_a_serial_device_at_its_topology() {
    let sysfs_path = PathBuf::from("/sys/devices/platform/usb/1-1.4");
    let observed = ObservedUsbBootloader {
        sysfs_path: sysfs_path.clone(),
        usb_id: "1d50:6177".to_owned(),
        manufacturer: "katapult".to_owned(),
        serial_device: None,
    };

    assert!(matches!(
        select_usb_bootloader(observed),
        Err(UsbBootloaderSelectionError::KatapultSerialDeviceMissing(path)) if path == sysfs_path
    ));
}
