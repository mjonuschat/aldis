//! Preparing a file for prebuilt-firmware MCUs, and the coordinator's refusal of a request made
//! for another MCU.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aldis::beacon::ImageError;
use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort};
use aldis::coordinator::{BuildCoordinator, CoordinatorError, FlashCoordinatorError};
use aldis::flash::request::{Protocol, Vendor};
use aldis::flash::system::PrebuiltLinks;
use aldis::moonraker::{Mcu, McuInventory, McuTransport};
use aldis::prebuilt::{ExplicitError, prepare_explicit};
use aldis::workspace::RunWorkspace;

fn mcu(name: &str, app: &str, version: &str, model: &str) -> Mcu {
    Mcu {
        name: name.to_owned(),
        app: Some(app.to_owned()),
        version: Some(version.to_owned()),
        mcu: model.to_owned(),
        canbus_frequency_hz: None,
        build_versions: None,
        transport: Some(McuTransport::Serial {
            device: format!("/dev/serial/by-id/usb-{name}-if00"),
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

fn beacon_file() -> Vec<u8> {
    let mut file = vec![0x5a; 32];
    for word in [0x2010_u16, 0xbea1, 0x04d8, 0x0100] {
        file.extend_from_slice(&word.to_le_bytes());
    }
    file.extend_from_slice(b"UFD");
    file.push(16);
    let crc = dfu_crc(&file);
    file.extend_from_slice(&crc.to_le_bytes());
    file
}

/// An application image linked above Cartographer's Katapult, embedding its identify dictionary.
fn cartographer_file(can: Option<u64>) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(&0x2000_8000_u32.to_le_bytes());
    image.extend_from_slice(&0x0800_2101_u32.to_le_bytes());
    image.resize(0x200, 0);
    let mut config = serde_json::json!({ "MCU": "stm32g431xx" });
    if let Some(can) = can {
        config["CANBUS_FREQUENCY"] = can.into();
    }
    let dictionary = serde_json::json!({ "version": "CARTOGRAPHER V4 6.1.0", "config": config });
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(dictionary.to_string().as_bytes())
        .unwrap();
    image.extend(encoder.finish().unwrap());
    image
}

fn file() -> &'static Path {
    Path::new("klipper.bin")
}

#[test]
fn takes_only_a_beacon_dfu_image_for_a_beacon() {
    let beacon = mcu("mcu beacon", "", "Beacon 2.0.1", "samd21g18a");

    assert_eq!(
        prepare_explicit(&beacon, file(), b"\x00\x10\x00\x20 plain binary"),
        Err(ExplicitError::NotBeaconImage(ImageError::NoSuffix))
    );
    let beta = mcu("mcu beacon", "", "Beacon 2.2.0-beta", "samd21g18a");
    assert_eq!(
        prepare_explicit(&beta, file(), b"\x00\x10\x00\x20 plain binary"),
        Err(ExplicitError::NotBeaconImage(ImageError::NoSuffix)),
        "a Beacon whose version aldis cannot compare still takes only a Beacon image"
    );

    let request = prepare_explicit(&beacon, file(), &beacon_file())
        .unwrap()
        .expect("a Beacon image for a Beacon");
    assert_eq!(request.vendor(), Vendor::Beacon);
    assert!(
        matches!(request.protocol(), Protocol::BeaconDfu { image } if image.usb_id() == "04d8:bea1")
    );
}

#[test]
fn takes_only_an_application_image_for_a_cartographers_own_mcu_and_link() {
    let probe = |version: &str, can: Option<u64>| {
        let mut probe = mcu("mcu probe", "Cartographer", version, "stm32g431xx");
        probe.canbus_frequency_hz = can;
        probe
    };

    assert_eq!(
        prepare_explicit(
            &probe("CARTOGRAPHER V4 6.0.0", Some(1_000_000)),
            file(),
            b"plain binary"
        ),
        Err(ExplicitError::Cartographer(
            "klipper.bin does not suit it: it holds no Cartographer identify dictionary".to_owned()
        ))
    );
    assert!(matches!(
        prepare_explicit(&probe("CARTOGRAPHER V3", None), file(), b"x"),
        Err(ExplicitError::Cartographer(reason)) if reason.contains("names no version number")
    ));
    assert!(matches!(
        prepare_explicit(
            &probe("CARTOGRAPHER V4 6.0.0", Some(500_000)),
            file(),
            &cartographer_file(Some(1_000_000))
        ),
        Err(ExplicitError::Cartographer(reason)) if reason.contains("does not suit it")
    ));

    let request = prepare_explicit(
        &probe("CARTOGRAPHER V4 6.0.0", Some(1_000_000)),
        file(),
        &cartographer_file(Some(1_000_000)),
    )
    .unwrap()
    .expect("a suitable image");
    assert_eq!(request.vendor(), Vendor::Cartographer);
    assert!(request.vendor().removes_calibration());
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            application_start: 0x0800_2000,
            links: PrebuiltLinks::UsbOrCan,
            ..
        }
    ));
}

#[test]
fn sends_foci_to_its_boards_katapult_over_usb_only() {
    let build_versions = Some("rust=1.98.1 features=none board=a82de21d".to_owned());
    let mut foci = mcu("mcu servo", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32h723xx");
    foci.build_versions = build_versions.clone();
    let request = prepare_explicit(&foci, file(), b"any image")
        .unwrap()
        .expect("FOCI takes any image");
    assert_eq!(request.vendor(), Vendor::Foci);
    assert!(!request.vendor().removes_calibration());
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            image,
            application_start: 0x0802_0000,
            links: PrebuiltLinks::UsbOnly,
        } if image == b"any image"
    ));

    let mut openffboard = mcu("mcu servo", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32f407xx");
    openffboard.build_versions = build_versions;
    assert!(matches!(
        prepare_explicit(&openffboard, file(), b"any image")
            .unwrap()
            .unwrap()
            .protocol(),
        Protocol::Katapult {
            application_start: 0x0800_8000,
            ..
        }
    ));

    let unknown = mcu("mcu servo", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32g474xx");
    assert_eq!(
        prepare_explicit(&unknown, file(), b"any image"),
        Err(ExplicitError::UnknownFociBoard("stm32g474xx".to_owned())),
        "a recognized MCU the firmware cannot be placed for never falls through"
    );
}

#[test]
fn lets_any_file_through_for_firmware_it_builds() {
    let klipper = mcu("mcu", "Klipper", "v0.13.0-770-gce7002bed", "stm32h723xx");
    assert_eq!(prepare_explicit(&klipper, file(), b"anything"), Ok(None));
}

#[derive(Clone, Default)]
struct RecordingRunner(Arc<Mutex<Vec<BuildCommand>>>);

impl CommandPort for RecordingRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.0.lock().unwrap().push(command.clone());
        Ok(CommandOutput {
            success: false,
            stdout: b"inactive\n".to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[test]
fn refuses_a_request_prepared_for_another_mcu_before_touching_klipper() {
    let probe = |name: &str, device: &str| {
        let mut probe = mcu(name, "Cartographer", "CARTOGRAPHER V4 6.0.0", "stm32g431xx");
        probe.transport = Some(McuTransport::Serial {
            device: device.to_owned(),
        });
        probe
    };
    let first = probe("mcu first", "/dev/serial/by-id/usb-Cartographer_1-if00");
    let second = probe("mcu second", "/dev/serial/by-id/usb-Cartographer_2-if00");
    let request = prepare_explicit(&first, file(), &cartographer_file(None))
        .unwrap()
        .unwrap();
    let inventory = McuInventory {
        mcus: vec![first, second],
        unreported: Vec::new(),
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
    let runner = RecordingRunner::default();
    let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
    let approved = coordinator
        .prepare(&inventory, &workspace, "mcu second", false)
        .unwrap()
        .approve();

    let error = coordinator
        .flash_prebuilt(
            approved,
            &request,
            aldis::update_run::standard_flash_options(),
            |_| {},
        )
        .unwrap_err();

    assert!(
        matches!(
            &error,
            FlashCoordinatorError::Coordinator(CoordinatorError::RequestForAnotherMcu {
                requested, approved
            }) if requested == "mcu first" && approved == "mcu second"
        ),
        "{error:?}"
    );
    assert!(
        runner.0.lock().unwrap().is_empty(),
        "Klipper is neither queried nor stopped"
    );
}

struct FakeReleases;

impl aldis::foci::ReleasePort for FakeReleases {
    fn latest_release(&self) -> Result<aldis::foci::Release, aldis::foci::ReleaseError> {
        panic!("a run never looks up a release");
    }

    fn download_firmware(
        &self,
        _: &aldis::foci::Release,
        _: &str,
    ) -> Result<Vec<u8>, aldis::foci::ReleaseError> {
        Ok(b"release image".to_vec())
    }
}

#[test]
fn prepares_each_automatic_update_for_the_protocol_its_vendor_allows() {
    use aldis::eligibility::{CheckoutRevision, TargetVersions};
    use aldis::prebuilt::prepare_automatic;

    let dir = tempfile::tempdir().unwrap();
    let beacon_path = dir.path().join("revh.dfu");
    std::fs::write(&beacon_path, beacon_file()).unwrap();
    let carto_path = dir.path().join("usb_full.bin");
    std::fs::write(&carto_path, cartographer_file(None)).unwrap();

    let mut foci = mcu("mcu foci", "FOCI", "v0.4.0-0-g0fb68e1b", "stm32h723xx");
    foci.build_versions = Some("rust=1.98.1 features=none board=a82de21d".to_owned());
    let beacon = mcu("mcu beacon", "", "Beacon 2.0.1", "samd21g18a");
    let probe = mcu(
        "mcu probe",
        "Cartographer",
        "CARTOGRAPHER V4 6.0.0",
        "stm32g431xx",
    );
    let klipper = mcu("mcu", "Klipper", "v0.13.0-770-gce7002bed", "stm32h723xx");

    let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
        .with_foci_release(aldis::foci::Release::from_tag("v0.4.1"))
        .with_beacon_targets(aldis::beacon::Targets {
            checkout: None,
            mcus: [(
                "mcu beacon".to_owned(),
                Ok(aldis::beacon::Target {
                    revision: "h".to_owned(),
                    path: beacon_path,
                    version: aldis::beacon::version_from_bcd(0x2010),
                }),
            )]
            .into(),
        })
        .with_cartographer_targets(aldis::cartographer::Targets {
            checkout: None,
            plugin: None,
            mcus: [(
                "mcu probe".to_owned(),
                Ok(aldis::cartographer::Target {
                    path: carto_path,
                    version: semver::Version::new(6, 1, 0),
                    reported: "CARTOGRAPHER V4 6.1.0".to_owned(),
                    held_back: None,
                }),
            )]
            .into(),
        });
    let inventory = McuInventory {
        mcus: vec![foci.clone(), beacon.clone(), probe.clone(), klipper.clone()],
        unreported: Vec::new(),
    };
    let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
    let runner = RecordingRunner::default();
    let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner);
    let prepare = |mcu: &Mcu| {
        let pending = coordinator
            .prepare(&inventory, &workspace, &mcu.name, false)
            .unwrap();
        prepare_automatic(mcu, &versions, &FakeReleases, &pending, &mut |_| {})
    };

    let request = prepare(&foci).unwrap().unwrap();
    assert_eq!(request.vendor(), Vendor::Foci);
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            image,
            application_start: 0x0802_0000,
            links: PrebuiltLinks::UsbOnly,
        } if image == b"release image"
    ));
    let request = prepare(&probe).unwrap().unwrap();
    assert_eq!(request.vendor(), Vendor::Cartographer);
    assert!(matches!(
        request.protocol(),
        Protocol::Katapult {
            application_start: 0x0800_2000,
            links: PrebuiltLinks::UsbOrCan,
            ..
        }
    ));
    let request = prepare(&beacon).unwrap().unwrap();
    assert!(matches!(request.protocol(), Protocol::BeaconDfu { .. }));
    assert!(
        prepare(&klipper).is_none(),
        "firmware aldis builds is not prepared here"
    );
}

#[test]
fn prepares_a_recovery_image_only_for_the_bootloader_it_names() {
    use aldis::beacon::Unavailable;
    use aldis::flash::beacon_dfu::ObservedBootloader;
    use aldis::prebuilt::prepare_beacon_recovery;

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("firmware")).unwrap();
    std::fs::write(dir.path().join("firmware/revh.dfu"), beacon_file()).unwrap();
    let bootloader = |product_id| ObservedBootloader {
        sysfs_path: "/nonexistent/1-1".into(),
        vendor_id: 0x04d8,
        product_id,
        product: "Beacon RevH Bootloader".to_owned(),
    };

    let request = prepare_beacon_recovery(Some(dir.path()), &bootloader(0xbea1)).unwrap();
    assert_eq!(request.path(), dir.path().join("firmware/revh.dfu"));
    assert_eq!(request.image().usb_id(), "04d8:bea1");

    assert!(matches!(
        prepare_beacon_recovery(Some(dir.path()), &bootloader(0xbeac)),
        Err(Unavailable::WrongBootloader { .. })
    ));
    assert!(matches!(
        prepare_beacon_recovery(None, &bootloader(0xbea1)),
        Err(Unavailable::NoCheckout)
    ));
}

#[test]
fn prepares_a_beacon_update_for_the_same_or_an_older_version() {
    use aldis::eligibility::{CheckoutRevision, TargetVersions};
    use aldis::prebuilt::prepare_automatic;

    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("revh.dfu");
    std::fs::write(&image, beacon_file()).unwrap(); // holds Beacon 2.1.0
    let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
        .with_beacon_targets(aldis::beacon::Targets {
            checkout: None,
            mcus: [(
                "mcu beacon".to_owned(),
                Ok(aldis::beacon::Target {
                    revision: "h".to_owned(),
                    path: image,
                    version: aldis::beacon::version_from_bcd(0x2010),
                }),
            )]
            .into(),
        });
    let beacon_at = |version: &str| mcu("mcu beacon", "", version, "samd21g18a");
    let inventory = McuInventory {
        mcus: vec![beacon_at("Beacon 2.2.0")],
        unreported: Vec::new(),
    };
    let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
    let runner = RecordingRunner::default();
    let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner);
    let pending = coordinator
        .prepare(&inventory, &workspace, "mcu beacon", false)
        .unwrap();
    let prepare = |running: &str| {
        prepare_automatic(
            &beacon_at(running),
            &versions,
            &FakeReleases,
            &pending,
            &mut |_| {},
        )
        .unwrap()
    };

    assert!(
        prepare("Beacon 2.1.0").is_ok(),
        "the same version may be reflashed"
    );
    assert!(prepare("Beacon 2.0.1").is_ok());
}

#[test]
fn says_why_an_update_leaves_a_newer_beacon_alone() {
    use aldis::eligibility::{CheckoutRevision, TargetVersions, downgrade_refusal};

    let versions = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
        .with_beacon_targets(aldis::beacon::Targets {
            checkout: None,
            mcus: [(
                "mcu beacon".to_owned(),
                Ok(aldis::beacon::Target {
                    revision: "h".to_owned(),
                    path: "/home/pi/beacon_klipper/firmware/revh.dfu".into(),
                    version: aldis::beacon::version_from_bcd(0x2010),
                }),
            )]
            .into(),
        });
    let beacon = |version: &str| mcu("mcu beacon", "", version, "samd21g18a");

    assert_eq!(
        downgrade_refusal(&beacon("Beacon 2.2.0"), &versions),
        Some(
            "mcu beacon runs Beacon 2.2.0, newer than the Beacon 2.1.0 in its beacon_klipper \
             checkout, and an update never downgrades it: name the file with `aldis flash \
             beacon <file>` to install the older image"
                .to_owned()
        )
    );
    for version in ["Beacon 2.1.0", "Beacon 2.0.1"] {
        assert_eq!(downgrade_refusal(&beacon(version), &versions), None);
    }
}
