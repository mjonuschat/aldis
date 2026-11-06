use std::path::{Path, PathBuf};

use aldis::build::BuildRequest;
use aldis::flash::system::{
    PrebuiltRoute, SerialFlashRoute, SystemFlashError, UnsupportedBootloaderContext,
    flash_prebuilt_katapult_with_progress, katapult_route, serial_route,
};
use aldis::flash::usb_bootloader::ObservedUsbBootloader;
use aldis::moonraker::McuTransport;
use aldis::prepare::PreparedBuild;

fn observed(usb_id: &str) -> ObservedUsbBootloader {
    ObservedUsbBootloader {
        sysfs_path: PathBuf::from("/sys/devices/pci0000:00/usb1/1-2"),
        usb_id: usb_id.to_owned(),
        manufacturer: String::new(),
        serial_device: Some(PathBuf::from("/dev/ttyACM0")),
    }
}

fn route(usb_id: &str, kconfig: &str) -> Result<SerialFlashRoute, SystemFlashError> {
    serial_route(
        observed(usb_id),
        kconfig,
        &UnsupportedBootloaderContext::default(),
    )
}

#[test]
fn routes_each_supported_serial_bootloader_from_its_observed_identity() {
    assert!(matches!(
        route("1d50:6177", ""),
        Ok(SerialFlashRoute::Katapult { serial_device }) if serial_device == Path::new("/dev/ttyACM0")
    ));
    assert!(matches!(
        route("0483:df11", "CONFIG_STM32_FLASH_START_2000=y\n"),
        Ok(SerialFlashRoute::Stm32Dfu { sysfs_path, target })
            if sysfs_path == Path::new("/sys/devices/pci0000:00/usb1/1-2")
                && target.application_start == 0x0800_2000
    ));
    assert!(matches!(
        route("2e8a:0003", ""),
        Ok(SerialFlashRoute::PicoBoot { sysfs_path })
            if sysfs_path == Path::new("/sys/devices/pci0000:00/usb1/1-2")
    ));
}

#[test]
fn routes_a_samd21_sam_ba_bootloader_with_the_application_address_from_kconfig() {
    assert!(matches!(
        route("2886:002f", "CONFIG_MACH_SAMD21G18=y\n"),
        Ok(SerialFlashRoute::SamBa { serial_device, target })
            if serial_device == Path::new("/dev/ttyACM0") && target.application_start == 0x2000
    ));
    assert!(matches!(
        route("2886:002f", "CONFIG_SAMD_FLASH_START_0000=y\n"),
        Err(SystemFlashError::SamBa(_))
    ));
}

#[test]
fn rejects_unknown_observed_bootloaders_before_selecting_a_route() {
    assert!(matches!(
        route("1234:5678", "CONFIG_STM32_FLASH_START_2000=y\n"),
        Err(SystemFlashError::Selection { .. })
    ));
}

#[test]
fn reports_the_unsupported_identity_and_transport_in_the_error_message() {
    let error = serial_route(
        observed("1234:5678"),
        "",
        &UnsupportedBootloaderContext {
            transport: "serial (/dev/serial/by-id/mcu)".to_owned(),
            reset_elapsed: None,
        },
    )
    .expect_err("an unrecognized identity should be rejected");

    let message = error.to_string();
    assert!(message.contains("USB device 1234:5678"));
    assert!(message.contains("sysfs path:   /sys/devices/pci0000:00/usb1/1-2"));
    assert!(message.contains("transport:    serial (/dev/serial/by-id/mcu)"));
}

#[test]
fn requires_a_valid_stm32_application_address_only_for_dfuse() {
    assert!(matches!(
        route("0483:df11", ""),
        Err(SystemFlashError::Stm32Target(_))
    ));
    assert!(matches!(
        route("2e8a:0003", ""),
        Ok(SerialFlashRoute::PicoBoot { .. })
    ));
    assert!(matches!(
        route("239a:000b", ""),
        Err(SystemFlashError::Selection { .. })
    ));
}

fn katapult_only(usb_id: &str) -> Result<PathBuf, SystemFlashError> {
    katapult_route(
        observed(usb_id),
        &UnsupportedBootloaderContext {
            transport: "/dev/serial/by-id/usb-foci_FOCI_Ouroboros-if00".to_owned(),
            reset_elapsed: None,
        },
    )
}

#[test]
fn routes_prebuilt_firmware_only_to_katapult() {
    assert_eq!(
        katapult_only("1d50:6177").unwrap(),
        Path::new("/dev/ttyACM0")
    );
    for (usb_id, bootloader) in [
        ("0483:df11", "STM32 DFU"),
        ("2e8a:0003", "PicoBoot"),
        ("2886:002f", "SAM-BA"),
    ] {
        let error = katapult_only(usb_id).unwrap_err();
        assert!(
            matches!(&error, SystemFlashError::NotKatapult { bootloader: seen, .. } if *seen == bootloader),
            "{usb_id}: {error:?}"
        );
        assert!(error.to_string().contains("aldis reboot"), "{error}");
    }
    assert!(matches!(
        katapult_only("1234:5678"),
        Err(SystemFlashError::Selection { .. })
    ));
}

fn prebuilt(transport: Option<McuTransport>) -> PreparedBuild {
    PreparedBuild {
        target_name: "mcu ouroboros".to_owned(),
        mcu: "stm32h723xx".to_owned(),
        transport,
        request: BuildRequest {
            kconfig: String::new(),
            config_path: PathBuf::from("/nonexistent/.config"),
            artifact_path: PathBuf::from("/nonexistent/firmware"),
            clean: false,
        },
    }
}

#[test]
fn refuses_prebuilt_firmware_over_can_before_touching_the_mcu() {
    let mut stages = Vec::new();
    let error = flash_prebuilt_katapult_with_progress(
        &prebuilt(Some(McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        })),
        &[0xab],
        0x0802_0000,
        aldis::update_run::standard_flash_options(),
        |stage| stages.push(stage),
    )
    .unwrap_err();

    assert!(
        matches!(&error, SystemFlashError::LinkRefused { route: PrebuiltRoute::KatapultUsb, transport } if transport == "CAN (can0, e7819ed8e7d3)"),
        "{error:?}"
    );
    assert!(stages.is_empty());
    assert!(matches!(
        flash_prebuilt_katapult_with_progress(
            &prebuilt(None),
            &[0xab],
            0x0802_0000,
            aldis::update_run::standard_flash_options(),
            |_| {},
        ),
        Err(SystemFlashError::MissingTransport)
    ));
}

fn beacon_image() -> aldis::beacon::FirmwareImage {
    aldis::beacon::FirmwareImage {
        payload: vec![0xab; 8],
        vendor_id: 0x04d8,
        product_id: 0xbea1,
        version: aldis::beacon::version_from_bcd(0x2010),
    }
}

#[test]
fn refuses_beacon_firmware_over_can_before_touching_the_mcu() {
    let mut stages = Vec::new();
    let error = aldis::flash::system::flash_beacon_with_progress(
        &prebuilt(Some(McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        })),
        &beacon_image(),
        aldis::update_run::standard_flash_options(),
        |stage| stages.push(stage),
    )
    .unwrap_err();

    assert!(
        matches!(&error, SystemFlashError::LinkRefused { route: PrebuiltRoute::BeaconUsb, transport } if transport == "CAN (can0, e7819ed8e7d3)"),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .starts_with("Beacon firmware is flashed only over USB")
    );
    assert!(stages.is_empty());
    assert!(
        aldis::coordinator::FlashCoordinatorError::Flash(error).allows_klipper_restore(),
        "an MCU that was never asked to enter its bootloader lets Klipper be restored"
    );
    assert!(matches!(
        aldis::flash::system::flash_beacon_with_progress(
            &prebuilt(None),
            &beacon_image(),
            aldis::update_run::standard_flash_options(),
            |_| {},
        ),
        Err(SystemFlashError::MissingTransport)
    ));
}

#[test]
fn explains_a_bootloader_the_beacon_image_is_not_for() {
    let error = SystemFlashError::BeaconImageMismatch {
        bootloader: "04d8:beac".to_owned(),
        image: "04d8:bea1".to_owned(),
    };
    let message = error.to_string();
    assert!(message.contains("04d8:beac"), "{message}");
    assert!(message.contains("update_firmware.py"), "{message}");
    assert!(
        !aldis::coordinator::FlashCoordinatorError::Flash(error).allows_klipper_restore(),
        "the Beacon is left in its bootloader"
    );
}

#[test]
fn refuses_a_beacon_bootloader_the_image_is_not_for_before_opening_it() {
    let bootloader = aldis::flash::beacon_dfu::ObservedBootloader {
        sysfs_path: PathBuf::from("/nonexistent/1-1"),
        vendor_id: 0x04d8,
        product_id: 0xbeac,
        product: "Beacon RevD Bootloader".to_owned(),
    };
    let mut stages = Vec::new();

    let error =
        aldis::flash::system::flash_beacon_bootloader(&bootloader, &beacon_image(), |stage| {
            stages.push(stage)
        })
        .unwrap_err();

    assert!(
        matches!(&error, SystemFlashError::BeaconImageMismatch { bootloader, image }
            if bootloader == "04d8:beac" && image == "04d8:bea1"),
        "{error:?}"
    );
    assert!(stages.is_empty());
}

#[test]
fn explains_prebuilt_firmware_refused_on_a_uart() {
    let uart = SystemFlashError::LinkRefused {
        route: PrebuiltRoute::KatapultUsb,
        transport: "the UART /dev/ttyAMA0".to_owned(),
    };
    assert_eq!(
        uart.to_string(),
        "this firmware is flashed only through Katapult on a USB serial device, and the MCU is \
         configured on the UART /dev/ttyAMA0"
    );
    assert!(
        aldis::coordinator::FlashCoordinatorError::Flash(uart).allows_klipper_restore(),
        "an MCU on a UART was never asked to enter its bootloader"
    );
}

#[test]
fn words_each_prebuilt_link_refusal_exactly() {
    let refused = |route| {
        SystemFlashError::LinkRefused {
            route,
            transport: "CAN (can0, e7819ed8e7d3)".to_owned(),
        }
        .to_string()
    };

    assert_eq!(
        refused(PrebuiltRoute::KatapultUsb),
        "this firmware is flashed only through Katapult on a USB serial device, and the MCU is \
         configured on CAN (can0, e7819ed8e7d3)"
    );
    assert_eq!(
        refused(PrebuiltRoute::BeaconUsb),
        "Beacon firmware is flashed only over USB, and the Beacon is configured on CAN (can0, \
         e7819ed8e7d3)"
    );
}
