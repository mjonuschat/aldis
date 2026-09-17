use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aldis::flash::reboot::{detect_bootloaders, wait_for_any_bootloader, wait_until_absent};
use aldis::flash::usb_bootloader::UsbBootloaderKind;

#[test]
fn detects_and_classifies_every_known_bootloader_under_a_root() {
    let root = unique_temporary_path();
    write_usb_device(&root, "1-1.2", "1d50:6177", "katapult");
    write_usb_device(&root, "1-1.3", "1209:0001", "example");

    let detected = detect_bootloaders(&root);

    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0].kind, UsbBootloaderKind::Katapult);
    assert_eq!(detected[0].observed.sysfs_path, root.join("1-1.2"));

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

#[test]
fn waits_until_a_device_directory_disappears() {
    let root = unique_temporary_path();
    write_usb_device(&root, "1-1.2", "1d50:6177", "katapult");
    let device_dir = root.join("1-1.2");

    let watched = device_dir.clone();
    let remover = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        fs::remove_dir_all(&watched).expect("remove device fixture");
    });

    let disappeared = wait_until_absent(
        &device_dir,
        Duration::from_millis(500),
        Duration::from_millis(5),
    );

    remover.join().expect("remover thread should not panic");
    assert!(disappeared);

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

#[test]
fn reports_still_present_when_the_device_never_disappears() {
    let root = unique_temporary_path();
    write_usb_device(&root, "1-1.2", "1d50:6177", "katapult");
    let device_dir = root.join("1-1.2");

    let disappeared = wait_until_absent(
        &device_dir,
        Duration::from_millis(30),
        Duration::from_millis(5),
    );

    assert!(!disappeared);

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

#[test]
fn waits_for_a_bootloader_to_appear_on_the_bus() {
    let root = unique_temporary_path();
    fs::create_dir_all(&root).expect("create sysfs fixture");

    let watched_root = root.clone();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        write_usb_device(&watched_root, "1-1.2", "1d50:6177", "katapult");
    });

    let detected =
        wait_for_any_bootloader(&root, Duration::from_millis(500), Duration::from_millis(5));

    writer.join().expect("writer thread should not panic");
    assert_eq!(detected.len(), 1);
    assert_eq!(detected[0].kind, UsbBootloaderKind::Katapult);

    fs::remove_dir_all(root).expect("remove sysfs fixture");
}

fn write_usb_device(root: &std::path::Path, name: &str, usb_id: &str, manufacturer: &str) {
    let device_dir = root.join(name);
    fs::create_dir_all(&device_dir).expect("create device fixture");
    let (vendor, product) = usb_id.split_once(':').expect("usb id has vendor:product");
    fs::write(device_dir.join("idVendor"), format!("{vendor}\n")).expect("write idVendor");
    fs::write(device_dir.join("idProduct"), format!("{product}\n")).expect("write idProduct");
    fs::write(device_dir.join("manufacturer"), format!("{manufacturer}\n"))
        .expect("write manufacturer");
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-reboot-{}-{nonce}", std::process::id()))
}
