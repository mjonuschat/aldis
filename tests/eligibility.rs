use aldis::eligibility::{
    CheckoutRevision, Eligibility, FirmwareKind, RevisionStatus, TargetVersions, UnsupportedReason,
    UpdateSelection, assess_mcu, is_selected, revisions_match,
};
use aldis::moonraker::Mcu;
use aldis::prebuilt::Running;

fn mcu(app: Option<&str>, version: Option<&str>, kconfig: &str) -> Mcu {
    Mcu {
        name: "mcu".to_owned(),
        app: app.map(str::to_owned),
        version: version.map(str::to_owned),
        mcu: "rp2040".to_owned(),
        canbus_frequency_hz: None,
        build_versions: None,
        transport: None,
        kconfig: kconfig.to_owned(),
    }
}

#[test]
fn classifies_supported_external_and_legacy_firmware_from_reported_metadata() {
    assert_eq!(
        assess_mcu(
            &mcu(Some("Klipper"), Some("v1"), "CONFIG=x\n"),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::Eligible
    );
    assert_eq!(
        assess_mcu(
            &mcu(Some("Beacon"), Some("v1"), "CONFIG=x\n"),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::ExternallyManaged("Beacon".to_owned())
    );
    assert_eq!(
        assess_mcu(
            &mcu(None, Some("v1"), ""),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::Unsupported(UnsupportedReason::NoEmbeddedConfig)
    );
}

#[test]
fn reports_a_klipper_version_below_the_minimum_as_unsupported() {
    assert_eq!(
        assess_mcu(
            &mcu(Some("Klipper"), Some("v0.12.0-500-gdeadbeef"), "CONFIG=x\n"),
            &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-753-g8c29c0a8e".to_owned()))
        )
        .eligibility,
        Eligibility::Unsupported(UnsupportedReason::VersionTooOld {
            firmware: FirmwareKind::Klipper,
            running: "v0.12.0-500-gdeadbeef".to_owned(),
            minimum: "v0.13.0-753-g8c29c0a8e",
        })
    );
}

#[test]
fn reports_a_kalico_version_below_the_minimum_as_unsupported() {
    assert_eq!(
        assess_mcu(
            &mcu(Some("Kalico"), Some("v2026.08.00-1-gabc"), "CONFIG=x\n"),
            &TargetVersions::klipper(CheckoutRevision::Known(
                "v2026.09.00-3-g6127720c4".to_owned()
            ))
        )
        .eligibility,
        Eligibility::Unsupported(UnsupportedReason::VersionTooOld {
            firmware: FirmwareKind::Kalico,
            running: "v2026.08.00-1-gabc".to_owned(),
            minimum: "v2026.09.00-3-g6127720c4",
        })
    );
}

#[test]
fn treats_a_klipper_version_at_or_above_the_minimum_as_eligible() {
    let status = assess_mcu(
        &mcu(
            Some("Klipper"),
            Some("v0.13.0-753-g8c29c0a8e"),
            "CONFIG=x\n",
        ),
        &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-753-g8c29c0a8e".to_owned())),
    );
    assert_eq!(status.eligibility, Eligibility::Eligible);

    let newer_tag = assess_mcu(
        &mcu(Some("Klipper"), Some("v0.14.0-5-gabc"), "CONFIG=x\n"),
        &TargetVersions::klipper(CheckoutRevision::Known("v0.14.0-5-gabc".to_owned())),
    );
    assert_eq!(newer_tag.eligibility, Eligibility::Eligible);
}

#[test]
fn names_third_party_firmware_from_its_version_when_app_is_blank() {
    // Older Cartographer builds report no `app` and name themselves in `mcu_version` instead
    // of a git-describe-shaped version.
    assert_eq!(
        assess_mcu(
            &mcu(Some(""), Some("CARTOGRAPHER V4 5.1.0"), ""),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::ExternallyManaged("CARTOGRAPHER".to_owned())
    );
    // A Beacon whose version cannot be compared stays externally managed.
    assert_eq!(
        assess_mcu(
            &mcu(Some(""), Some("Beacon 2.1.0-beta"), ""),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::ExternallyManaged("Beacon".to_owned())
    );
}

#[test]
fn treats_a_blank_app_with_a_git_describe_version_as_unidentified_legacy_firmware() {
    assert_eq!(
        assess_mcu(
            &mcu(Some(""), Some("v0.10.0-100-gdeadbeef"), ""),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::Unsupported(UnsupportedReason::NoEmbeddedConfig)
    );
}

#[test]
fn accepts_mainline_klipper_metadata_without_an_app_field() {
    let status = assess_mcu(
        &mcu(None, Some("v0.13.0"), "CONFIG_MACH_STM32=y\n"),
        &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0".to_owned())),
    );

    assert_eq!(status.eligibility, Eligibility::Eligible);
    assert_eq!(status.revision, Some(RevisionStatus::Current));
}

#[test]
fn compares_only_known_eligible_revisions() {
    let current = assess_mcu(
        &mcu(Some("Kalico"), Some("v1"), "CONFIG=x\n"),
        &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned())),
    );
    assert_eq!(current.revision, Some(RevisionStatus::Current));
    let outdated = assess_mcu(
        &mcu(Some("Klipper"), Some("v0"), "CONFIG=x\n"),
        &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned())),
    );
    assert_eq!(outdated.revision, Some(RevisionStatus::UpdateRequired));
    let unknown = assess_mcu(
        &mcu(Some("Klipper"), None, "CONFIG=x\n"),
        &TargetVersions::indeterminate(),
    );
    assert_eq!(unknown.revision, Some(RevisionStatus::Indeterminate));
}

#[test]
fn tolerates_a_differently_abbreviated_commit_hash() {
    assert!(revisions_match(
        "v0.13.0-762-g9871eeef1",
        "v0.13.0-762-g9871eeef"
    ));
    assert!(revisions_match(
        "v0.13.0-762-g9871eeef",
        "v0.13.0-762-g9871eeef1"
    ));
    assert!(!revisions_match(
        "v0.13.0-762-g9871eeef1",
        "v0.13.0-762-gdeadbeef"
    ));
}

#[test]
fn requires_matching_dirty_state() {
    assert!(!revisions_match(
        "v0.13.0-762-g9871eeef-dirty",
        "v0.13.0-762-g9871eeef"
    ));
    assert!(revisions_match(
        "v0.13.0-762-g9871eeef1-dirty",
        "v0.13.0-762-g9871eeef-dirty"
    ));
}

#[test]
fn tolerates_klippers_full_dirty_build_suffix_on_the_running_mcu() {
    assert!(revisions_match(
        "v0.13.0-762-g9871eeef-dirty-20260918_104500-myhost",
        "v0.13.0-762-g9871eeef-dirty"
    ));
    assert!(revisions_match(
        "v0.13.0-762-g9871eeef-dirty-20260918_104500-my-host-1",
        "v0.13.0-762-g9871eeef1-dirty"
    ));
}

#[test]
fn still_rejects_a_mismatched_hash_under_the_full_dirty_build_suffix() {
    assert!(!revisions_match(
        "v0.13.0-762-gdeadbeef-dirty-20260918_104500-myhost",
        "v0.13.0-762-g9871eeef-dirty"
    ));
}

#[test]
fn still_requires_matching_dirty_state_against_the_full_build_suffix() {
    assert!(!revisions_match(
        "v0.13.0-762-g9871eeef-dirty-20260918_104500-myhost",
        "v0.13.0-762-g9871eeef"
    ));
}

#[test]
fn treats_a_flashed_mcu_at_the_firmwares_own_abbreviation_length_as_current() {
    let status = assess_mcu(
        &mcu(
            Some("Klipper"),
            Some("v0.13.0-762-g9871eeef1"),
            "CONFIG=x\n",
        ),
        &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-762-g9871eeef".to_owned())),
    );
    assert_eq!(status.revision, Some(RevisionStatus::Current));
}

#[test]
fn selects_only_outdated_eligible_mcus_unless_explicitly_overridden() {
    let current = mcu(Some("Klipper"), Some("v1"), "CONFIG=x\n");
    let outdated = mcu(Some("Klipper"), Some("v0"), "CONFIG=x\n");
    let external = mcu(Some("Beacon"), Some("v0"), "CONFIG=x\n");
    let checkout = TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()));
    assert!(!is_selected(
        &current,
        &checkout,
        &UpdateSelection::Required
    ));
    assert!(is_selected(
        &outdated,
        &checkout,
        &UpdateSelection::Required
    ));
    assert!(is_selected(&current, &checkout, &UpdateSelection::All));
    assert!(!is_selected(&external, &checkout, &UpdateSelection::All));
}

#[test]
fn rejects_mcu_families_without_a_flashable_bootloader() {
    let samd = mcu(
        Some("Klipper"),
        Some("v0.13.0-770-gce7002bed"),
        "CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAMD51G19=y\nCONFIG_CLOCK_REF_INTERNAL=y\n",
    );
    let status = assess_mcu(
        &samd,
        &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned())),
    );
    assert_eq!(
        status.eligibility,
        Eligibility::UnsupportedMcu("ATSAMD".to_owned())
    );
    assert_eq!(status.revision, None);
}

#[test]
fn rejects_the_samd21_parts_whose_ram_cannot_stage_a_transfer() {
    for part in ["SAMD21E15", "SAMD21E16", "SAMD21G17"] {
        let status = assess_mcu(
            &mcu(
                Some("Klipper"),
                Some("v0.13.0-770-gce7002bed"),
                &format!("CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAMD21=y\nCONFIG_MACH_{part}=y\n"),
            ),
            &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned())),
        );
        assert_eq!(
            status.eligibility,
            Eligibility::UnsupportedMcu("ATSAMD".to_owned()),
            "{part}"
        );
    }
}

#[test]
fn accepts_every_family_with_a_flashable_bootloader() {
    for kconfig in [
        "CONFIG_MACH_STM32=y\nCONFIG_MACH_STM32H723=y\n",
        "CONFIG_MACH_RPXXXX=y\nCONFIG_MACH_RP2040=y\n",
        "CONFIG_MACH_RP2040=y\n",
        "CONFIG_MACH_LPC176X=y\n",
        "CONFIG_MACH_LINUX=y\n",
        "CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAMD21G18=y\n",
    ] {
        let status = assess_mcu(
            &mcu(Some("Klipper"), Some("v0.13.0-770-gce7002bed"), kconfig),
            &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned())),
        );
        assert_eq!(status.eligibility, Eligibility::Eligible, "{kconfig}");
    }
}

#[test]
fn keeps_a_configuration_naming_no_family_eligible() {
    let status = assess_mcu(
        &mcu(
            Some("Klipper"),
            Some("v0.13.0-770-gce7002bed"),
            "CONFIG_TEST=y\n",
        ),
        &TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned())),
    );
    assert_eq!(status.eligibility, Eligibility::Eligible);
}

#[test]
fn never_selects_an_mcu_family_without_a_flashable_bootloader() {
    let samd = mcu(
        Some("Klipper"),
        Some("v0.13.0-753-g8c29c0a8e"),
        "CONFIG_MACH_ATSAMD=y\n",
    );
    let checkout =
        TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned()));

    assert!(!is_selected(&samd, &checkout, &UpdateSelection::Required));
    assert!(!is_selected(&samd, &checkout, &UpdateSelection::All));
}

fn foci(version: &str, build_versions: &str) -> Mcu {
    Mcu {
        name: "mcu ouroboros".to_owned(),
        app: Some("FOCI".to_owned()),
        version: Some(version.to_owned()),
        mcu: "stm32h723xx".to_owned(),
        canbus_frequency_hz: None,
        build_versions: Some(build_versions.to_owned()),
        transport: None,
        kconfig: String::new(),
    }
}

const FOCI_PROD: &str = "rust=1.98.1 features=none board=a82de21d";

fn foci_release(tag: &str) -> TargetVersions {
    TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned()))
        .with_foci_release(Some(aldis::foci::Release::from_tag(tag).unwrap()))
}

#[test]
fn compares_a_foci_release_build_against_the_foci_release() {
    let running = foci("v0.4.0-0-g0fb68e1b", FOCI_PROD);

    let outdated = assess_mcu(&running, &foci_release("v0.4.1"));
    assert!(matches!(
        outdated.eligibility,
        Eligibility::Prebuilt(Running::Foci(_))
    ));
    assert_eq!(outdated.revision, Some(RevisionStatus::UpdateRequired));
    assert_eq!(
        assess_mcu(&running, &foci_release("v0.4.0")).revision,
        Some(RevisionStatus::Current)
    );
    assert_eq!(
        assess_mcu(
            &running,
            &TargetVersions::klipper(CheckoutRevision::Known("v0.4.0-0-g0fb68e1b".to_owned()))
        )
        .revision,
        Some(RevisionStatus::Indeterminate),
        "the Klipper checkout is no target for FOCI"
    );
}

#[test]
fn reports_a_foci_build_no_release_matches_as_unsupported() {
    let status = assess_mcu(
        &foci(
            "v0.4.1-0-ga82de21d",
            "rust=1.98.1 features=dev board=a82de21d",
        ),
        &foci_release("v0.4.1"),
    );

    assert_eq!(
        status.eligibility,
        Eligibility::Unsupported(UnsupportedReason::Prebuilt(
            aldis::prebuilt::Unsupported::Foci(aldis::foci::Unsupported::UnknownFlavor(
                "dev".to_owned()
            ))
        ))
    );
    assert_eq!(status.revision, None);
}

#[test]
fn selects_foci_mcus_like_klipper_ones() {
    let outdated = foci("v0.4.0-0-g0fb68e1b", FOCI_PROD);
    let current = foci("v0.4.1-0-ga82de21d", FOCI_PROD);
    let versions = foci_release("v0.4.1");

    assert!(is_selected(
        &outdated,
        &versions,
        &UpdateSelection::Required
    ));
    assert!(!is_selected(
        &current,
        &versions,
        &UpdateSelection::Required
    ));
    assert!(is_selected(&current, &versions, &UpdateSelection::All));
    assert!(
        !is_selected(
            &current,
            &TargetVersions::indeterminate(),
            &UpdateSelection::All
        ),
        "a FOCI MCU needs a known release to be updated to"
    );
}

#[test]
fn targets_and_confirms_foci_mcus_by_the_foci_release() {
    let versions = foci_release("v0.4.1");
    let klipper = mcu(
        Some("Klipper"),
        Some("v0.13.0-770-gce7002bed"),
        "CONFIG=x\n",
    );

    assert_eq!(
        versions.label_for(&foci("v0.4.0-0-g0fb68e1b", FOCI_PROD)),
        "v0.4.1"
    );
    assert_eq!(versions.label_for(&klipper), "v0.13.0-770-gce7002bed");
    assert_eq!(
        versions.reached_by(&foci("v0.4.1-0-ga82de21d", FOCI_PROD)),
        Some(true)
    );
    assert_eq!(
        versions.reached_by(&foci("v0.4.0-0-g0fb68e1b", FOCI_PROD)),
        Some(false)
    );
    assert_eq!(versions.reached_by(&klipper), Some(true));

    let unknown = TargetVersions::indeterminate();
    assert_eq!(
        unknown.label_for(&foci("v0.4.0-0-g0fb68e1b", FOCI_PROD)),
        "unknown"
    );
    assert_eq!(
        unknown.reached_by(&foci("v0.4.0-0-g0fb68e1b", FOCI_PROD)),
        None
    );
}

fn beacon(version: &str) -> Mcu {
    let mut beacon = mcu(Some(""), Some(version), "");
    beacon.name = "mcu beacon".to_owned();
    beacon
}

fn beacon_target(bcd: u16) -> TargetVersions {
    let target = aldis::beacon::Target {
        revision: "h".to_owned(),
        path: std::path::PathBuf::from("/home/pi/beacon_klipper/firmware/revh.dfu"),
        version: aldis::beacon::version_from_bcd(bcd),
    };
    TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned()))
        .with_beacon_targets(aldis::beacon::Targets {
            checkout: Some(std::path::PathBuf::from("/home/pi/beacon_klipper")),
            mcus: [("mcu beacon".to_owned(), Ok(target))].into(),
        })
}

#[test]
fn compares_a_beacon_with_the_image_for_its_revision() {
    let running = beacon("Beacon 2.0.1");

    let outdated = assess_mcu(&running, &beacon_target(0x2010));
    assert_eq!(
        outdated.eligibility,
        Eligibility::Prebuilt(Running::Beacon(aldis::beacon::Firmware {
            version: aldis::beacon::version_from_bcd(0x2001)
        }))
    );
    assert_eq!(outdated.revision, Some(RevisionStatus::UpdateRequired));
    assert_eq!(
        assess_mcu(&running, &beacon_target(0x2001)).revision,
        Some(RevisionStatus::Current)
    );
    assert_eq!(
        assess_mcu(&running, &beacon_target(0x2000)).revision,
        Some(RevisionStatus::Indeterminate),
        "an older image is never an update"
    );
    let unknown = TargetVersions::klipper(CheckoutRevision::Known("Beacon 2.0.1".to_owned()));
    assert_eq!(
        assess_mcu(&running, &unknown).revision,
        Some(RevisionStatus::Indeterminate),
        "the Klipper checkout is no target for a Beacon"
    );
}

#[test]
fn selects_a_beacon_only_with_a_known_image() {
    let outdated = beacon("Beacon 2.0.1");
    let current = beacon("Beacon 2.1.0");
    let versions = beacon_target(0x2010);

    assert!(is_selected(
        &outdated,
        &versions,
        &UpdateSelection::Required
    ));
    assert!(!is_selected(
        &current,
        &versions,
        &UpdateSelection::Required
    ));
    assert!(is_selected(&current, &versions, &UpdateSelection::All));
    assert!(!is_selected(
        &current,
        &TargetVersions::indeterminate(),
        &UpdateSelection::All
    ));
    let mut other = outdated.clone();
    other.name = "mcu beacon sensor rear".to_owned();
    assert!(
        !is_selected(&other, &versions, &UpdateSelection::All),
        "each Beacon needs its own image"
    );
}

#[test]
fn targets_and_confirms_a_beacon_by_its_image_version() {
    let versions = beacon_target(0x2010);

    assert_eq!(versions.label_for(&beacon("Beacon 2.0.1")), "Beacon 2.1.0");
    assert_eq!(versions.reached_by(&beacon("Beacon 2.1.0")), Some(true));
    assert_eq!(versions.reached_by(&beacon("Beacon 2.0.1")), Some(false));

    let unknown = TargetVersions::indeterminate();
    assert_eq!(unknown.label_for(&beacon("Beacon 2.0.1")), "unknown");
    assert_eq!(unknown.reached_by(&beacon("Beacon 2.0.1")), None);
}

/// The selection of every prebuilt vendor with the running firmware older than, equal to and
/// newer than its target, under each selection. Frozen from the behavior before the shared
/// prebuilt-firmware path, so that path cannot change who gets offered or forced an update.
/// Each row is `(required, all)`.
#[test]
fn selects_prebuilt_vendors_by_how_the_running_version_compares_with_the_target() {
    let selections = |mcu: &Mcu, versions: &TargetVersions| {
        (
            is_selected(mcu, versions, &UpdateSelection::Required),
            is_selected(mcu, versions, &UpdateSelection::All),
        )
    };

    // FOCI (running v0.4.0 / v0.4.1 / v0.4.2 against the v0.4.1 release).
    let release = foci_release("v0.4.1");
    assert_eq!(
        selections(&foci("v0.4.0-0-g0fb68e1b", FOCI_PROD), &release),
        (true, true)
    );
    assert_eq!(
        selections(&foci("v0.4.1-0-g0fb68e1b", FOCI_PROD), &release),
        (false, true)
    );
    assert_eq!(
        selections(&foci("v0.4.2-0-g0fb68e1b", FOCI_PROD), &release),
        (false, true)
    );
    assert_eq!(
        selections(&foci("v0.4.0-3-gabcdef12", FOCI_PROD), &release),
        (false, true),
        "a development build is never offered, but --all still reaches it"
    );

    // Beacon (running 2.0.1 / 2.1.0 / 2.2.0 against an image holding 2.1.0).
    let image = beacon_target(0x2010);
    assert_eq!(selections(&beacon("Beacon 2.0.1"), &image), (true, true));
    assert_eq!(selections(&beacon("Beacon 2.1.0"), &image), (false, true));
    // No silent Beacon downgrade: a newer running Beacon is never offered the older image, even
    // by --all or --force; only `aldis flash` with the file itself installs it.
    assert_eq!(selections(&beacon("Beacon 2.2.0"), &image), (false, false));
}
