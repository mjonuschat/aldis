use aldis::eligibility::RevisionStatus;
use aldis::foci::{Board, Flavor, Release, Unsupported, Version, compare, identify, is_foci};
use aldis::moonraker::Mcu;

fn foci_mcu(mcu: &str, version: &str, build_versions: Option<&str>) -> Mcu {
    Mcu {
        name: "mcu ouroboros".to_owned(),
        app: Some("FOCI".to_owned()),
        version: Some(version.to_owned()),
        mcu: mcu.to_owned(),
        canbus_frequency_hz: None,
        build_versions: build_versions.map(str::to_owned),
        transport: None,
        kconfig: String::new(),
    }
}

const PROD: &str =
    "rust=1.98.1 features=none board=a82de21d fw=1f2e3d4c tmc=5a6b7c8d ankyra=9e0f1a2b";

fn version(text: &str) -> Version {
    Version::parse(text).unwrap_or_else(|| panic!("{text:?} should parse"))
}

#[test]
fn recognizes_foci_by_its_app_name() {
    assert!(is_foci(&foci_mcu(
        "stm32h723xx",
        "v0.4.1-0-ga82de21d",
        None
    )));
    let mut klipper = foci_mcu("stm32h723xx", "v0.13.0-770-gce7002bed", None);
    klipper.app = Some("Klipper".to_owned());
    assert!(!is_foci(&klipper));
    klipper.app = None;
    assert!(!is_foci(&klipper));
}

#[test]
fn identifies_the_board_flavor_and_version_of_a_release_build() {
    let firmware = identify(&foci_mcu("stm32h723xx", "v0.4.1-0-ga82de21d", Some(PROD))).unwrap();

    assert_eq!(firmware.board, Board::Ouroboros);
    assert_eq!(firmware.flavor, Flavor::Prod);
    assert_eq!(firmware.version.package, semver::Version::new(0, 4, 1));
    assert!(firmware.version.is_release_build());

    let openffboard = identify(&foci_mcu(
        "stm32f407xx",
        "v0.4.1-0-g67cd8c5b",
        Some("rust=1.98.1 features=trace board=67cd8c5b"),
    ))
    .unwrap();
    assert_eq!(openffboard.board, Board::OpenFfboard);
    assert_eq!(openffboard.flavor, Flavor::Trace);
}

#[test]
fn refuses_builds_no_release_asset_matches() {
    assert_eq!(
        identify(&foci_mcu("stm32g474xx", "v0.4.1-0-ga82de21d", Some(PROD))),
        Err(Unsupported::UnknownBoard("stm32g474xx".to_owned()))
    );
    assert_eq!(
        identify(&foci_mcu(
            "stm32h723xx",
            "v0.4.1-0-ga82de21d",
            Some("rust=1.98.1 features=trace,trace-current-path board=a82de21d"),
        )),
        Err(Unsupported::UnknownFlavor(
            "trace,trace-current-path".to_owned()
        ))
    );
    assert_eq!(
        identify(&foci_mcu("stm32h723xx", "v0.4.1-0-ga82de21d", None)),
        Err(Unsupported::NoBuildFeatures)
    );
    assert_eq!(
        identify(&foci_mcu(
            "stm32h723xx",
            "v0.4.1-0-ga82de21d",
            Some("rust=1.98.1")
        )),
        Err(Unsupported::NoBuildFeatures)
    );
    assert_eq!(
        identify(&foci_mcu("stm32h723xx", "0.4.1", Some(PROD))),
        Err(Unsupported::UnparsableVersion("0.4.1".to_owned()))
    );
}

#[test]
fn parses_pre_release_dev_and_dirty_versions() {
    let rc = version("v0.4.0-rc.1-0-g0fb68e1b");
    assert_eq!(rc.package, semver::Version::parse("0.4.0-rc.1").unwrap());
    assert!(rc.is_release_build());

    let dev = version("v0.4.1-3-gdeadbeef");
    assert_eq!(dev.commits, 3);
    assert!(!dev.is_release_build());

    let dirty = version("v0.4.1-0-ga82de21d-dirty");
    assert!(dirty.dirty);
    assert!(!dirty.is_release_build());

    // build.rs falls back to "unknown" when git describe fails.
    assert_eq!(version("v0.4.1-0-gunknown").commits, 0);

    for unparsable in [
        "",
        "v0.4.1",
        "v0.4.1-0",
        "v0.4.1-x-gabc",
        "v0.4-0-gabc",
        "v0.4.1-0-g",
    ] {
        assert_eq!(Version::parse(unparsable), None, "{unparsable:?}");
    }
}

#[test]
fn names_release_assets_as_the_release_workflow_does() {
    let release = Release::from_tag("v0.4.1").unwrap();

    assert_eq!(
        release.asset_name(Board::Ouroboros, Flavor::Prod),
        "ouroboros-fw-v0.4.1-prod.bin"
    );
    assert_eq!(
        release.asset_name(Board::OpenFfboard, Flavor::Trace),
        "openffboard-fw-v0.4.1-trace.bin"
    );
    assert_eq!(Release::from_tag("0.4.1"), None);
    assert_eq!(Release::from_tag("nightly"), None);
}

#[test]
fn compares_only_release_builds_against_a_release() {
    let release = Release::from_tag("v0.4.1").unwrap();

    assert_eq!(
        compare(&version("v0.4.1-0-ga82de21d"), &release),
        RevisionStatus::Current
    );
    assert_eq!(
        compare(&version("v0.4.0-0-g0fb68e1b"), &release),
        RevisionStatus::UpdateRequired
    );
    assert_eq!(
        compare(&version("v0.4.1-rc.1-0-g0fb68e1b"), &release),
        RevisionStatus::UpdateRequired
    );
    assert_eq!(
        compare(&version("v0.5.0-0-gabcdef12"), &release),
        RevisionStatus::Indeterminate
    );
    assert_eq!(
        compare(&version("v0.4.0-7-gabcdef12"), &release),
        RevisionStatus::Indeterminate
    );
    assert_eq!(
        compare(&version("v0.4.0-0-gabcdef12-dirty"), &release),
        RevisionStatus::Indeterminate
    );
}

#[test]
fn verifies_firmware_against_its_published_sha256_file() {
    use aldis::foci::{ReleaseError, verify_firmware};
    use sha2::{Digest, Sha256};

    let asset = "ouroboros-fw-v0.4.1-prod.bin";
    let firmware = b"firmware image".as_slice();
    let digest: String = Sha256::digest(firmware)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let checksum_file = format!("{digest}  {asset}\n");

    assert!(verify_firmware(asset, &checksum_file, firmware).is_ok());
    assert!(matches!(
        verify_firmware(asset, &checksum_file, b"tampered"),
        Err(ReleaseError::ChecksumMismatch(file)) if file == asset
    ));
    assert!(matches!(
        verify_firmware(asset, &format!("{digest}  openffboard-fw-v0.4.1-prod.bin\n"), firmware),
        Err(ReleaseError::ChecksumInvalid(file)) if file == asset
    ));
    assert!(matches!(
        verify_firmware(asset, "", firmware),
        Err(ReleaseError::ChecksumInvalid(_))
    ));
}

#[test]
fn hands_out_a_downloaded_image_only_after_checking_its_published_digest() {
    use aldis::foci::{ReleaseError, download_verified};
    use sha2::{Digest, Sha256};
    use std::cell::RefCell;

    let asset = "ouroboros-fw-v0.4.1-prod.bin";
    let digest: String = Sha256::digest(b"firmware image")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let asked = RefCell::new(Vec::new());
    let fetch = |image: &'static [u8]| {
        let asked = &asked;
        let digest = digest.clone();
        move || {
            download_verified(
                asset,
                |name| {
                    asked.borrow_mut().push(name.to_owned());
                    Ok(format!("{digest}  {asset}\n"))
                },
                |name| {
                    asked.borrow_mut().push(name.to_owned());
                    Ok(image.to_vec())
                },
            )
        }
    };

    assert_eq!(fetch(b"firmware image")().unwrap(), b"firmware image");
    assert_eq!(
        *asked.borrow(),
        vec![
            "ouroboros-fw-v0.4.1-prod.bin.sha256".to_owned(),
            "ouroboros-fw-v0.4.1-prod.bin".to_owned()
        ]
    );
    assert!(matches!(
        fetch(b"tampered")(),
        Err(ReleaseError::ChecksumMismatch(file)) if file == asset
    ));
}

#[test]
fn links_each_board_above_the_katapult_foci_documents_for_it() {
    assert_eq!(Board::Ouroboros.application_start(), 0x0802_0000);
    assert_eq!(Board::OpenFfboard.application_start(), 0x0800_8000);
}
