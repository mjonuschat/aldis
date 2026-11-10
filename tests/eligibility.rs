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
    // Firmware that reports no `app` names itself in `mcu_version` instead of a
    // git-describe-shaped version.
    assert_eq!(
        assess_mcu(
            &mcu(Some(""), Some("EDDYPROBE 1.2.0"), ""),
            &TargetVersions::klipper(CheckoutRevision::Known("v1".to_owned()))
        )
        .eligibility,
        Eligibility::ExternallyManaged("EDDYPROBE".to_owned())
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
fn tolerates_klippers_build_suffix_on_a_clean_revision_from_a_tree_without_git() {
    assert!(revisions_match(
        "v0.13.0-770-gce7002bed-20261108_190244-randall",
        "v0.13.0-770-gce7002bed"
    ));
    assert!(revisions_match(
        "v0.13.0-770-gce7002bed-20261108_190244-my-host-1",
        "v0.13.0-770-gce7002bed1"
    ));
    assert!(revisions_match(
        "v0.13.0-770-gce7002bed1-20261108_190244-randall",
        "v0.13.0-770-gce7002bed"
    ));
}

#[test]
fn compares_the_tag_and_commit_count_exactly_and_only_the_hash_by_prefix() {
    assert!(!revisions_match(
        "abcdef-20261108_190244-board-1-g1111111",
        "abcdef-20261108_190244-board-2-g2222222"
    ));
    assert!(!revisions_match(
        "abcdef-20261108_190244-board-1-g1111111-20261109_101500-randall",
        "abcdef-20261108_190244-board-2-g1111111"
    ));
    assert!(!revisions_match(
        "v0.13.0-77-gce7002bed",
        "v0.13.0-770-gce7002bed"
    ));
    assert!(!revisions_match(
        "v0.13.0-770-gce7002bed",
        "v0.13.1-770-gce7002bed"
    ));
    assert!(revisions_match("v1-good-3-gabc1234", "v1-good-3-gabc12345"));
}

#[test]
fn matches_a_timestamp_shaped_tag_against_itself_with_or_without_a_build_suffix() {
    let tagged = "abcdef-20261108_190244-board-1-g1111111";
    assert!(revisions_match(tagged, tagged));
    assert!(revisions_match(
        &format!("{tagged}-20261109_101500-randall"),
        tagged
    ));
    assert!(revisions_match(
        &format!("{tagged}-dirty-20261109_101500-my-host"),
        &format!("{tagged}-dirty")
    ));
}

#[test]
fn does_not_take_a_timestamp_inside_a_tag_for_a_build_suffix() {
    assert!(!revisions_match(
        "release-20261108_190244-board-1-gabc1234",
        "release-20261108_190244-board-2-gdef5678"
    ));
    assert!(revisions_match(
        "release-20261108_190244-board-1-gabc1234-20261109_101500-randall",
        "release-20261108_190244-board-1-gabc1234"
    ));
}

#[test]
fn tolerates_the_build_suffix_on_a_checkout_without_a_reachable_tag() {
    assert!(revisions_match(
        "ce7002bed-dirty-20261108_190244-randall",
        "ce7002bed-dirty"
    ));
    assert!(revisions_match(
        "ce7002bed-20261108_190244-randall",
        "ce7002bed1"
    ));
    assert!(!revisions_match(
        "ce7002bed-dirty-20261108_190244-randall",
        "ce7002bed"
    ));
    assert!(!revisions_match(
        "ce7002bed-20261108_190244-randall",
        "deadbeef0"
    ));
}

#[test]
fn still_rejects_another_revision_or_dirty_state_under_the_plain_build_suffix() {
    assert!(!revisions_match(
        "v0.13.0-770-gce7002bed-20261108_190244-randall",
        "v0.13.0-771-gabcdef012"
    ));
    assert!(
        !revisions_match(
            "v0.13.0-770-gce7002bed-20261108_190244-randall",
            "v0.13.0-770-gce7002bed-dirty"
        ),
        "a clean build is not a dirty checkout"
    );
    assert!(
        !revisions_match(
            "v0.13.0-770-gce7002bed-20261108_190244-randall",
            "v0.13.0-770-gce7002b0000"
        ),
        "the same prefix length is not enough; the hashes must agree"
    );
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
fn accepts_the_indx_toolboard_but_not_other_same51_parts() {
    let versions =
        TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned()));
    let indx = mcu(
        Some("Kalico"),
        Some("v2026.11.00-30-g991e1823"),
        "CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAME51J19=y\nCONFIG_CLOCK_REF_X25M_XOSC0=y\n\
         CONFIG_WANT_INDX_HEATER=y\n",
    );
    assert_eq!(
        assess_mcu(&indx, &versions).eligibility,
        Eligibility::Eligible
    );

    let other = mcu(
        Some("Klipper"),
        Some("v0.13.0-770-gce7002bed"),
        "CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAME51J19=y\nCONFIG_CLOCK_REF_X25M_XOSC0=y\n",
    );
    assert_eq!(
        assess_mcu(&other, &versions).eligibility,
        Eligibility::UnsupportedMcu("ATSAMD".to_owned())
    );
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
        Some(RevisionStatus::Newer),
        "a newer Beacon needs no update"
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

fn cartographer(name: &str, version: &str) -> Mcu {
    let mut probe = mcu(Some("Cartographer"), Some(version), "");
    probe.name = name.to_owned();
    probe.mcu = "stm32g431xx".to_owned();
    probe
}

fn cartographer_target(reported: &str) -> TargetVersions {
    let version = aldis::cartographer::parse_version(reported).unwrap();
    TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-770-gce7002bed".to_owned()))
        .with_cartographer_targets(aldis::cartographer::Targets {
            checkout: Some(std::path::PathBuf::from("/home/pi/cartographer_firmware")),
            plugin: None,
            mcus: [(
                "mcu cartographer".to_owned(),
                Ok(aldis::cartographer::Target {
                    path: std::path::PathBuf::from("/home/pi/cartographer_firmware/x.bin"),
                    version,
                    reported: reported.to_owned(),
                    held_back: None,
                }),
            )]
            .into(),
        })
}

#[test]
fn compares_a_cartographer_with_its_image_and_never_downgrades_it() {
    let versions = cartographer_target("CARTOGRAPHER V4 6.1.0");

    let outdated = assess_mcu(
        &cartographer("mcu cartographer", "CARTOGRAPHER V4 6.0.0"),
        &versions,
    );
    assert!(matches!(
        outdated.eligibility,
        Eligibility::Prebuilt(Running::Cartographer(_))
    ));
    assert_eq!(outdated.revision, Some(RevisionStatus::UpdateRequired));
    assert_eq!(
        assess_mcu(
            &cartographer("mcu cartographer", "CARTOGRAPHER V4 6.1.0"),
            &versions
        )
        .revision,
        Some(RevisionStatus::Current)
    );
    assert_eq!(
        assess_mcu(
            &cartographer("mcu cartographer", "CARTOGRAPHER v4 6.2.0"),
            &versions
        )
        .revision,
        Some(RevisionStatus::Newer)
    );
    assert_eq!(
        assess_mcu(
            &cartographer("mcu cartographer", "CARTOGRAPHER V4 6.0.0"),
            &TargetVersions::indeterminate()
        )
        .revision,
        Some(RevisionStatus::Indeterminate)
    );
}

#[test]
fn reports_a_cartographer_no_image_can_be_matched_to_as_unsupported() {
    let status = assess_mcu(
        &cartographer("mcu cartographer", "CARTOGRAPHER V3"),
        &TargetVersions::indeterminate(),
    );

    assert_eq!(
        status.eligibility,
        Eligibility::Unsupported(UnsupportedReason::Prebuilt(
            aldis::prebuilt::Unsupported::Cartographer(
                aldis::cartographer::Unsupported::UnparsableVersion("CARTOGRAPHER V3".to_owned())
            )
        ))
    );
    assert_eq!(status.revision, None);
}

#[test]
fn selects_a_cartographer_only_with_a_known_image() {
    let outdated = cartographer("mcu cartographer", "CARTOGRAPHER V4 6.0.0");
    let versions = cartographer_target("CARTOGRAPHER V4 6.1.0");

    assert!(is_selected(
        &outdated,
        &versions,
        &UpdateSelection::Required
    ));
    assert!(!is_selected(
        &outdated,
        &TargetVersions::indeterminate(),
        &UpdateSelection::All
    ));
}

#[test]
fn confirms_a_cartographer_by_the_exact_version_its_image_reports() {
    let versions = cartographer_target("CARTOGRAPHER V4 6.1.0 lite");

    assert_eq!(
        versions.label_for(&cartographer(
            "mcu cartographer",
            "CARTOGRAPHER V4 6.0.0 Lite"
        )),
        "CARTOGRAPHER V4 6.1.0 lite"
    );
    assert_eq!(
        versions.reached_by(&cartographer(
            "mcu cartographer",
            "CARTOGRAPHER V4 6.1.0 lite"
        )),
        Some(true)
    );
    assert_eq!(
        versions.reached_by(&cartographer("mcu cartographer", "CARTOGRAPHER V4 6.1.0")),
        Some(false),
        "the full build is not the lite one"
    );
    assert_eq!(
        TargetVersions::indeterminate()
            .reached_by(&cartographer("mcu cartographer", "CARTOGRAPHER V4 6.1.0")),
        None
    );
}

/// The selection of every prebuilt vendor with the running firmware older than, equal to and
/// newer than its target, under each selection. An update never downgrades firmware, so a newer
/// running version is selected by no selection; only `aldis flash` with the file itself does.
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
        (false, false),
        "no selection replaces a newer build with an older release"
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
    assert_eq!(selections(&beacon("Beacon 2.2.0"), &image), (false, false));

    // Cartographer (running 6.0.0 / 6.1.0 / 6.2.0 against an image reporting 6.1.0).
    let image = cartographer_target("CARTOGRAPHER V4 6.1.0");
    let probe = |version: &str| cartographer("mcu cartographer", version);
    assert_eq!(
        selections(&probe("CARTOGRAPHER V4 6.0.0"), &image),
        (true, true)
    );
    assert_eq!(
        selections(&probe("CARTOGRAPHER V4 6.1.0"), &image),
        (false, true)
    );
    assert_eq!(
        selections(&probe("CARTOGRAPHER V4 6.2.0"), &image),
        (false, false)
    );
}

#[test]
fn compares_any_ordered_version_the_same_way() {
    use aldis::eligibility::compare_versions;

    assert_eq!(compare_versions(&1, &2), RevisionStatus::UpdateRequired);
    assert_eq!(compare_versions(&2, &2), RevisionStatus::Current);
    assert_eq!(
        compare_versions(&3, &2),
        RevisionStatus::Newer,
        "a newer running version needs no update"
    );
}

#[test]
fn turns_a_klipper_revision_into_a_version_that_orders_like_its_history() {
    use aldis::eligibility::klipper_version;
    use semver::Version;

    let version = |text: &str| klipper_version(text).unwrap_or_else(|| panic!("{text:?}"));

    assert_eq!(version("v0.13.0-0-g2d7717e3"), Version::new(0, 13, 0));
    assert_eq!(version("v0.13.0"), Version::new(0, 13, 0));
    assert_eq!(
        version("v0.13.0-756-g2d7717e3").to_string(),
        "0.13.1-dev.756"
    );
    // The hash and the dirty marker carry no order, however the build spells them.
    assert_eq!(
        version("v0.13.0-756-g2d7717e3-dirty"),
        version("v0.13.0-756-gabcdef0")
    );
    assert_eq!(
        version("v0.13.0-756-g2d7717e3-dirty-20250102_153012-printer"),
        version("v0.13.0-756-g2d7717e3")
    );

    let history = [
        "v0.12.0-900-gaaaaaaa",
        "v0.13.0-0-gbbbbbbb",
        "v0.13.0-9-gccccccc",
        "v0.13.0-756-gddddddd",
        "v0.13.1-0-geeeeeee",
        "v0.13.1-2-gfffffff",
    ];
    for pair in history.windows(2) {
        assert!(version(pair[0]) < version(pair[1]), "{pair:?}");
    }

    for unparsable in [
        "2d7717e3",
        "0.13.0-1-gabc",
        "v0.13-1-gabc",
        "v0.13.0-rc1-3-gabc",
        "",
    ] {
        assert_eq!(klipper_version(unparsable), None, "{unparsable:?}");
    }
}

#[test]
fn compares_a_klipper_mcu_with_the_checkout_it_is_built_from() {
    let at = |checkout: &str, running: &str| {
        assess_mcu(
            &mcu(Some("Klipper"), Some(running), "CONFIG=x\n"),
            &TargetVersions::klipper(CheckoutRevision::Known(checkout.to_owned())),
        )
        .revision
    };
    let checkout = "v0.13.0-900-g2d7717e3";

    assert_eq!(
        at(checkout, "v0.13.0-900-g2d7717e"),
        Some(RevisionStatus::Current),
        "the same commit, whatever the length of its hash"
    );
    assert_eq!(
        at(checkout, "v0.13.0-800-g1111111"),
        Some(RevisionStatus::UpdateRequired),
        "an older MCU is updated"
    );
    assert_eq!(
        at(checkout, "v0.13.0-901-g1111111"),
        Some(RevisionStatus::Newer),
        "a newer MCU needs no update, and is not replaced by one"
    );
    assert_eq!(
        at(checkout, "v0.13.1-0-g1111111"),
        Some(RevisionStatus::Newer)
    );
    assert_eq!(
        at(checkout, "v0.13.0-900-g1111111"),
        Some(RevisionStatus::UpdateRequired),
        "another commit at the same position is not the checkout's"
    );
    assert_eq!(
        at(checkout, "v0.13.0-900-g2d7717e3-dirty"),
        Some(RevisionStatus::UpdateRequired),
        "a dirty build is not the clean checkout"
    );
    assert_eq!(
        at("g2d7717e3", "v0.13.0-880-g1111111"),
        Some(RevisionStatus::UpdateRequired),
        "a revision with no tag has no order, so a difference is an update"
    );
}

#[test]
fn never_selects_a_klipper_mcu_for_an_older_build_than_it_runs() {
    use aldis::eligibility::downgrade_refusal;

    let versions =
        TargetVersions::klipper(CheckoutRevision::Known("v0.13.0-900-g2d7717e3".to_owned()));
    let newer = mcu(Some("Klipper"), Some("v0.13.0-950-g1111111"), "CONFIG=x\n");
    let older = mcu(Some("Klipper"), Some("v0.13.0-800-g1111111"), "CONFIG=x\n");

    for selection in [UpdateSelection::Required, UpdateSelection::All] {
        assert!(!is_selected(&newer, &versions, &selection));
    }
    assert!(is_selected(&older, &versions, &UpdateSelection::Required));
    assert!(is_selected(&older, &versions, &UpdateSelection::All));
    for selection in [UpdateSelection::Required, UpdateSelection::All] {
        assert!(aldis::eligibility::is_selected_allowing_downgrade(
            &newer, &versions, &selection, true
        ));
        assert!(!aldis::eligibility::is_selected_allowing_downgrade(
            &newer, &versions, &selection, false
        ));
    }
    assert!(aldis::eligibility::is_selected_allowing_downgrade(
        &older,
        &versions,
        &UpdateSelection::Required,
        true
    ));
    assert_eq!(
        downgrade_refusal(&newer, &versions),
        Some(
            "mcu runs v0.13.0-950-g1111111, newer than the v0.13.0-900-g2d7717e3 checkout, and \
             an update never downgrades it: update the checkout first"
                .to_owned()
        )
    );
    assert_eq!(downgrade_refusal(&older, &versions), None);
}
