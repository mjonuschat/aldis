use std::path::Path;

use aldis::beacon::{
    ImageError, Target, Unavailable, config_section, identify, image_path, is_beacon,
    locate_checkout, parse_image, parse_reported, reported, resolve_targets, revision_from_product,
    version_from_bcd,
};
use aldis::eligibility::{RevisionStatus, compare_versions};
use aldis::moonraker::{Mcu, McuInventory, McuTransport};

fn beacon_mcu(name: &str, app: Option<&str>, version: &str) -> Mcu {
    Mcu {
        name: name.to_owned(),
        app: app.map(str::to_owned),
        version: Some(version.to_owned()),
        mcu: "samd21g18a".to_owned(),
        canbus_frequency_hz: None,
        build_versions: None,
        transport: Some(McuTransport::Serial {
            device: format!("/dev/serial/by-id/usb-Beacon_{name}-if00"),
        }),
        kconfig: String::new(),
    }
}

/// CRC-32 without its final inversion, as a DFU suffix stores it.
fn dfu_crc(bytes: &[u8]) -> u32 {
    bytes.iter().fold(u32::MAX, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| {
            if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            }
        })
    })
}

/// A DFU file: `payload` followed by a suffix naming `vendor:product` and version `bcd`.
fn dfu_file(payload: &[u8], vendor: u16, product: u16, bcd: u16) -> Vec<u8> {
    let mut file = payload.to_vec();
    file.extend_from_slice(&bcd.to_le_bytes());
    file.extend_from_slice(&product.to_le_bytes());
    file.extend_from_slice(&vendor.to_le_bytes());
    file.extend_from_slice(&0x0100_u16.to_le_bytes());
    file.extend_from_slice(b"UFD");
    file.push(16);
    let crc = dfu_crc(&file);
    file.extend_from_slice(&crc.to_le_bytes());
    file
}

#[test]
fn parses_the_version_beacon_reports() {
    assert_eq!(
        parse_reported("Beacon 2.1.0"),
        Some(semver::Version::new(2, 1, 0))
    );
    assert_eq!(
        parse_reported("beacon  2.10.3 ").map(|version| version.to_string()),
        Some("2.10.3".to_owned())
    );
    for unparsable in [
        "Beacon",
        "Beacon 2.1",
        "Beacon 2.1.0.1",
        "Beacon 2.1.0 beta",
        "Beacon v2.1.0",
        "Beacon 2.1.x",
        "Beacon 2.1.256",
        "Cartographer 2.1.0",
        "v0.13.0-770-gce7002bed",
    ] {
        assert_eq!(parse_reported(unparsable), None, "{unparsable}");
    }
}

#[test]
fn decodes_the_version_a_dfu_suffix_holds_as_upstream_does() {
    assert_eq!(version_from_bcd(0x2010).to_string(), "2.1.0");
    assert_eq!(version_from_bcd(0x2100).to_string(), "2.10.0");
    assert_eq!(version_from_bcd(0x1093).to_string(), "1.9.3");
    assert_eq!(reported(&version_from_bcd(0x2010)), "Beacon 2.1.0");
}

#[test]
fn recognizes_beacon_by_its_version_with_a_blank_or_beacon_app() {
    let firmware = identify(&beacon_mcu("mcu beacon", Some(""), "Beacon 2.1.0")).unwrap();
    assert_eq!(firmware.version.to_string(), "2.1.0");
    assert!(is_beacon(&beacon_mcu("mcu beacon", None, "Beacon 2.1.0")));
    assert!(is_beacon(&beacon_mcu(
        "mcu beacon",
        Some("Beacon"),
        "Beacon 2.1.0"
    )));

    assert!(!is_beacon(&beacon_mcu(
        "mcu beacon",
        Some("Klipper"),
        "Beacon 2.1.0"
    )));
    assert!(!is_beacon(&beacon_mcu(
        "mcu beacon",
        Some("Beacon"),
        "v1.2.3"
    )));
    assert!(!is_beacon(&beacon_mcu("mcu beacon", Some(""), "Beacon")));
}

#[test]
fn compares_a_beacon_version_with_the_image_in_the_checkout() {
    let version = |bcd| version_from_bcd(bcd);
    assert_eq!(
        compare_versions(&version(0x2010), &version(0x2010)),
        RevisionStatus::Current
    );
    assert_eq!(
        compare_versions(&version(0x2000), &version(0x2010)),
        RevisionStatus::UpdateRequired
    );
    assert_eq!(
        compare_versions(&version(0x2010), &version(0x2000)),
        RevisionStatus::Indeterminate,
        "an older checkout is not a reason to downgrade"
    );
}

#[test]
fn names_the_settings_section_of_each_beacon_mcu_object() {
    assert_eq!(config_section("mcu beacon"), Some("beacon"));
    assert_eq!(
        config_section("mcu beacon sensor rear"),
        Some("beacon sensor rear")
    );
    assert_eq!(config_section("mcu beacon_other"), None);
    assert_eq!(config_section("mcu"), None);
    assert_eq!(config_section("mcu toolhead"), None);
}

#[test]
fn reads_the_revision_from_the_usb_product_name() {
    assert_eq!(revision_from_product("Beacon RevH"), Some("h".to_owned()));
    assert_eq!(
        revision_from_product("Beacon RevD Contact"),
        Some("d".to_owned())
    );
    assert_eq!(revision_from_product("Beacon revh"), Some("h".to_owned()));
    assert_eq!(revision_from_product("Beacon Rev"), None);
    assert_eq!(revision_from_product("Beacon Rev../x"), None);
    assert_eq!(
        revision_from_product("Beacon RevH Bootloader"),
        Some("h".to_owned())
    );
    assert_eq!(revision_from_product("Klipper samd21"), None);
}

#[test]
fn parses_a_dfu_image_and_strips_its_suffix() {
    let payload = [0x5a_u8; 300];
    let image = parse_image(&dfu_file(&payload, 0x04d8, 0xbea1, 0x2010)).unwrap();

    assert_eq!(image.payload, payload);
    assert_eq!(image.vendor_id, 0x04d8);
    assert_eq!(image.product_id, 0xbea1);
    assert_eq!(image.usb_id(), "04d8:bea1");
    assert_eq!(image.version.to_string(), "2.1.0");
}

#[test]
fn refuses_a_file_that_is_not_a_beacon_dfu_image() {
    let good = dfu_file(&[1, 2, 3, 4], 0x04d8, 0xbeac, 0x2010);

    assert_eq!(parse_image(&good[..10]), Err(ImageError::TooShort));

    let mut corrupt = good.clone();
    corrupt[0] ^= 0xff;
    assert_eq!(parse_image(&corrupt), Err(ImageError::ChecksumMismatch));

    let mut unsigned = good.clone();
    let signature = unsigned.len() - 8;
    unsigned[signature] = b'X';
    assert_eq!(parse_image(&unsigned), Err(ImageError::NoSuffix));

    assert_eq!(
        parse_image(&dfu_file(&[1, 2, 3, 4], 0x0483, 0xdf11, 0x2010)),
        Err(ImageError::WrongVendor(0x0483))
    );
    assert_eq!(
        parse_image(&dfu_file(&[], 0x04d8, 0xbeac, 0x2010)),
        Err(ImageError::Empty)
    );
}

fn checkout_with(dir: &Path, revision: &str, file: &[u8]) -> std::path::PathBuf {
    let checkout = dir.join("beacon_klipper");
    std::fs::create_dir_all(checkout.join("firmware")).unwrap();
    std::fs::write(image_path(&checkout, revision), file).unwrap();
    checkout
}

#[test]
fn finds_the_checkout_beacon_py_is_linked_from_or_the_one_in_home() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = checkout_with(dir.path(), "h", b"");
    std::fs::write(checkout.join("beacon.py"), "").unwrap();
    let klipper = dir.path().join("klipper");
    std::fs::create_dir_all(klipper.join("klippy/extras")).unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    assert_eq!(locate_checkout(None, Some(&klipper), Some(&home)), None);

    std::os::unix::fs::symlink(
        checkout.join("beacon.py"),
        klipper.join("klippy/extras/beacon.py"),
    )
    .unwrap();
    assert_eq!(
        locate_checkout(None, Some(&klipper), Some(&home)),
        Some(std::fs::canonicalize(&checkout).unwrap())
    );

    let in_home = checkout_with(&home, "d", b"");
    assert_eq!(
        locate_checkout(None, None, Some(&home)),
        Some(in_home.clone())
    );
    assert_eq!(
        locate_checkout(Some(&in_home), Some(&klipper), None),
        Some(in_home),
        "a configured checkout wins"
    );
    assert_eq!(
        locate_checkout(Some(&klipper), Some(&klipper), Some(&home)),
        None,
        "a configured directory without firmware is not replaced by another"
    );
}

#[test]
fn matches_each_beacon_to_the_image_for_its_revision() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = checkout_with(dir.path(), "h", &dfu_file(&[7; 64], 0x04d8, 0xbea1, 0x2010));
    let mut klipper = beacon_mcu("mcu", Some("Klipper"), "v0.13.0-770-gce7002bed");
    klipper.kconfig = "CONFIG_MACH_STM32=y\n".to_owned();
    let mut can = beacon_mcu("mcu beacon sensor can", Some(""), "Beacon 2.0.0");
    can.transport = Some(McuTransport::Can {
        interface: "can0".to_owned(),
        uuid: 0xe781_9ed8_e7d3,
    });
    let mut unconfigured = beacon_mcu("mcu beacon sensor none", Some(""), "Beacon 2.0.0");
    unconfigured.transport = None;
    let inventory = McuInventory {
        mcus: vec![
            klipper,
            beacon_mcu("mcu beacon", Some(""), "Beacon 2.0.0"),
            beacon_mcu("mcu beacon sensor revd", Some(""), "Beacon 2.0.0"),
            beacon_mcu("mcu beacon sensor odd", Some(""), "Beacon 2.0.0"),
            beacon_mcu("mcu beacon sensor gone", Some(""), "Beacon 2.0.0"),
            can,
            unconfigured,
        ],
        unreported: Vec::new(),
    };
    let product = |device: &Path| {
        let device = device.to_string_lossy();
        if device.contains("revd") {
            Some("Beacon RevD".to_owned())
        } else if device.contains("odd") {
            Some("Beacon Classic".to_owned())
        } else if device.contains("gone") {
            None
        } else {
            Some("Beacon RevH".to_owned())
        }
    };

    let targets = resolve_targets(&inventory, Some(&checkout), product);

    assert_eq!(targets.checkout.as_deref(), Some(checkout.as_path()));
    assert!(
        !targets.mcus.contains_key("mcu"),
        "only Beacons get targets"
    );
    assert_eq!(
        targets.target("mcu beacon"),
        Some(&Target {
            revision: "h".to_owned(),
            path: image_path(&checkout, "h"),
            version: version_from_bcd(0x2010),
        })
    );
    assert!(matches!(
        targets.mcus["mcu beacon sensor revd"],
        Err(Unavailable::Unreadable { .. })
    ));
    assert_eq!(
        targets.mcus["mcu beacon sensor odd"],
        Err(Unavailable::UnknownRevision("Beacon Classic".to_owned()))
    );
    assert!(matches!(
        targets.mcus["mcu beacon sensor gone"],
        Err(Unavailable::NoProduct(_))
    ));
    assert_eq!(
        targets.mcus["mcu beacon sensor can"],
        Err(Unavailable::NotUsb("CAN (can0, e7819ed8e7d3)".to_owned()))
    );
    assert_eq!(
        targets.mcus["mcu beacon sensor none"],
        Err(Unavailable::NotConfigured)
    );

    let without = resolve_targets(&inventory, None, product);
    assert_eq!(without.checkout, None);
    assert_eq!(without.mcus["mcu beacon"], Err(Unavailable::NoCheckout));
}

#[test]
fn reports_a_corrupt_image_in_the_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = dfu_file(&[7; 64], 0x04d8, 0xbea1, 0x2010);
    file[3] ^= 1;
    let checkout = checkout_with(dir.path(), "h", &file);
    let inventory = McuInventory {
        mcus: vec![beacon_mcu("mcu beacon", Some(""), "Beacon 2.0.0")],
        unreported: Vec::new(),
    };

    let targets = resolve_targets(&inventory, Some(&checkout), |_| {
        Some("Beacon RevH".to_owned())
    });

    let error = targets.mcus["mcu beacon"].clone().unwrap_err();
    assert_eq!(
        error,
        Unavailable::InvalidImage {
            path: image_path(&checkout, "h"),
            reason: ImageError::ChecksumMismatch,
        }
    );
    assert!(error.to_string().ends_with(
        "revh.dfu is not a Beacon firmware image: its DFU suffix checksum does not match its contents"
    ));
}

#[test]
fn names_a_beacon_even_when_its_version_cannot_be_compared() {
    use aldis::beacon::names_itself_beacon;

    assert!(names_itself_beacon(&beacon_mcu(
        "mcu beacon",
        Some(""),
        "Beacon 2.1.0"
    )));
    assert!(names_itself_beacon(&beacon_mcu(
        "mcu beacon",
        None,
        "Beacon 2.2.0-beta"
    )));
    assert!(names_itself_beacon(&beacon_mcu(
        "mcu beacon",
        Some("Beacon"),
        "BEACON"
    )));
    assert!(!names_itself_beacon(&beacon_mcu(
        "mcu beacon",
        Some("Klipper"),
        "Beacon 2.1.0"
    )));
    assert!(!names_itself_beacon(&beacon_mcu(
        "mcu beacon",
        Some(""),
        "Cartographer 2.1.0"
    )));
}

#[test]
fn picks_the_recovery_image_for_a_stuck_bootloader_by_its_revision() {
    use aldis::beacon::recovery_image;

    let dir = tempfile::tempdir().unwrap();
    let checkout = checkout_with(dir.path(), "h", &dfu_file(&[7; 64], 0x04d8, 0xbea1, 0x2010));

    let (path, image) =
        recovery_image(Some(&checkout), "Beacon RevH Bootloader", "04d8:bea1").unwrap();
    assert_eq!(path, image_path(&checkout, "h"));
    assert_eq!(image.version.to_string(), "2.1.0");

    assert_eq!(
        recovery_image(Some(&checkout), "Beacon RevH Bootloader", "04d8:beac").unwrap_err(),
        Unavailable::WrongBootloader {
            path: image_path(&checkout, "h"),
            image: "04d8:bea1".to_owned(),
            bootloader: "04d8:beac".to_owned(),
        }
    );
    assert!(matches!(
        recovery_image(Some(&checkout), "Beacon RevD Bootloader", "04d8:beac"),
        Err(Unavailable::Unreadable { .. })
    ));
    assert_eq!(
        recovery_image(Some(&checkout), "Bootloader", "04d8:bea1").unwrap_err(),
        Unavailable::UnknownRevision("Bootloader".to_owned())
    );
    assert_eq!(
        recovery_image(None, "Beacon RevH Bootloader", "04d8:bea1").unwrap_err(),
        Unavailable::NoCheckout
    );
}
