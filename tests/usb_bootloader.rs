use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::flash::usb_bootloader::{
    ObservedUsbBootloader, SelectedUsbBootloader, UnsupportedBootloaderIdentity, UsbBootloaderKind,
    UsbBootloaderSelectionError, classify_usb_identity, scan_usb_bootloaders,
    select_usb_bootloader,
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
        Err(UsbBootloaderSelectionError::Unsupported(_))
    ));
}

#[test]
fn captures_topology_diagnostics_for_an_unsupported_bootloader() {
    let root = unique_temporary_path();
    let device_dir = root.join("1-1.5");
    fs::create_dir_all(&device_dir).expect("create device fixture");
    fs::write(device_dir.join("bDeviceClass"), "ef\n").expect("write bDeviceClass");
    fs::write(device_dir.join("bDeviceSubClass"), "02\n").expect("write bDeviceSubClass");
    fs::write(device_dir.join("bDeviceProtocol"), "01\n").expect("write bDeviceProtocol");
    fs::write(device_dir.join("product"), "Example Device\n").expect("write product");

    let observed = ObservedUsbBootloader {
        sysfs_path: device_dir.clone(),
        usb_id: "1209:0001".to_owned(),
        manufacturer: "example".to_owned(),
        serial_device: None,
    };

    let error =
        select_usb_bootloader(observed).expect_err("an unrecognized identity should be rejected");
    assert_eq!(
        error,
        UsbBootloaderSelectionError::Unsupported(Box::new(UnsupportedBootloaderIdentity {
            usb_id: "1209:0001".to_owned(),
            manufacturer: "example".to_owned(),
            product: "Example Device".to_owned(),
            serial: "none".to_owned(),
            device_class: "ef".to_owned(),
            device_subclass: "02".to_owned(),
            device_protocol: "01".to_owned(),
            sysfs_path: device_dir.display().to_string(),
        }))
    );

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

#[test]
fn falls_back_to_unknown_and_none_for_missing_product_and_serial() {
    let root = unique_temporary_path();
    let device_dir = root.join("1-1.6");
    fs::create_dir_all(&device_dir).expect("create device fixture");

    let observed = ObservedUsbBootloader {
        sysfs_path: device_dir,
        usb_id: "1209:0002".to_owned(),
        manufacturer: "example".to_owned(),
        serial_device: None,
    };

    let error =
        select_usb_bootloader(observed).expect_err("an unrecognized identity should be rejected");
    assert!(matches!(
        error,
        UsbBootloaderSelectionError::Unsupported(identity)
            if identity.product == "unknown" && identity.serial == "none"
    ));

    fs::remove_dir_all(root).expect("remove sysfs fixture");
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

#[test]
fn scans_a_sysfs_root_for_every_classified_bootloader_identity_and_ignores_the_rest() {
    let root = unique_temporary_path();
    write_usb_device(&root, "1-1.2", "1d50:6177", "katapult", Some("ttyACM0"));
    write_usb_device(&root, "1-1.3", "0483:df11", "stmicroelectronics", None);
    write_usb_device(&root, "1-1.4", "1209:0001", "example", None);

    let mut observed = scan_usb_bootloaders(&root);
    observed.sort_by(|a, b| a.sysfs_path.cmp(&b.sysfs_path));

    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0].sysfs_path, root.join("1-1.2"));
    assert_eq!(observed[0].usb_id, "1d50:6177");
    assert_eq!(
        observed[0].serial_device,
        Some(PathBuf::from("/dev/ttyACM0"))
    );
    assert_eq!(observed[1].sysfs_path, root.join("1-1.3"));
    assert_eq!(observed[1].usb_id, "0483:df11");
    assert_eq!(observed[1].serial_device, None);

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

fn write_usb_device(
    root: &std::path::Path,
    name: &str,
    usb_id: &str,
    manufacturer: &str,
    tty: Option<&str>,
) {
    let device_dir = root.join(name);
    fs::create_dir_all(&device_dir).expect("create device fixture");
    let (vendor, product) = usb_id.split_once(':').expect("usb id has vendor:product");
    fs::write(device_dir.join("idVendor"), format!("{vendor}\n")).expect("write idVendor");
    fs::write(device_dir.join("idProduct"), format!("{product}\n")).expect("write idProduct");
    fs::write(device_dir.join("manufacturer"), format!("{manufacturer}\n"))
        .expect("write manufacturer");
    if let Some(tty) = tty {
        let interface = device_dir.join(format!("{name}:1.0"));
        fs::create_dir_all(interface.join("tty").join(tty)).expect("create tty fixture");
    }
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "aldis-usb-bootloader-{}-{nonce}",
        std::process::id()
    ))
}
