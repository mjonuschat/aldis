use aldis::eligibility::{
    CheckoutRevision, Eligibility, FirmwareKind, RevisionStatus, UnsupportedReason,
    UpdateSelection, assess_mcu, is_selected, revisions_match,
};
use aldis::moonraker::Mcu;

fn mcu(app: Option<&str>, version: Option<&str>, kconfig: &str) -> Mcu {
    Mcu {
        name: "mcu".to_owned(),
        app: app.map(str::to_owned),
        version: version.map(str::to_owned),
        mcu: "rp2040".to_owned(),
        canbus_frequency_hz: None,
        transport: None,
        kconfig: kconfig.to_owned(),
    }
}

#[test]
fn classifies_supported_external_and_legacy_firmware_from_reported_metadata() {
    assert_eq!(
        assess_mcu(
            &mcu(Some("Klipper"), Some("v1"), "CONFIG=x\n"),
            &CheckoutRevision::Known("v1".to_owned())
        )
        .eligibility,
        Eligibility::Eligible
    );
    assert_eq!(
        assess_mcu(
            &mcu(Some("Beacon"), Some("v1"), "CONFIG=x\n"),
            &CheckoutRevision::Known("v1".to_owned())
        )
        .eligibility,
        Eligibility::ExternallyManaged("Beacon".to_owned())
    );
    assert_eq!(
        assess_mcu(
            &mcu(None, Some("v1"), ""),
            &CheckoutRevision::Known("v1".to_owned())
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
            &CheckoutRevision::Known("v0.13.0-753-g8c29c0a8e".to_owned())
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
            &CheckoutRevision::Known("v2026.09.00-3-g6127720c4".to_owned())
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
        &CheckoutRevision::Known("v0.13.0-753-g8c29c0a8e".to_owned()),
    );
    assert_eq!(status.eligibility, Eligibility::Eligible);

    let newer_tag = assess_mcu(
        &mcu(Some("Klipper"), Some("v0.14.0-5-gabc"), "CONFIG=x\n"),
        &CheckoutRevision::Known("v0.14.0-5-gabc".to_owned()),
    );
    assert_eq!(newer_tag.eligibility, Eligibility::Eligible);
}

#[test]
fn names_third_party_firmware_from_its_version_when_app_is_blank() {
    // Beacon reports `app` as an empty string rather than omitting it, and names itself in
    // `mcu_version` (e.g. "Beacon 2.1.0") instead of a git-describe-shaped version.
    assert_eq!(
        assess_mcu(
            &mcu(Some(""), Some("Beacon 2.1.0"), ""),
            &CheckoutRevision::Known("v1".to_owned())
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
            &CheckoutRevision::Known("v1".to_owned())
        )
        .eligibility,
        Eligibility::Unsupported(UnsupportedReason::NoEmbeddedConfig)
    );
}

#[test]
fn accepts_mainline_klipper_metadata_without_an_app_field() {
    let status = assess_mcu(
        &mcu(None, Some("v0.13.0"), "CONFIG_MACH_ATSAMD=y\n"),
        &CheckoutRevision::Known("v0.13.0".to_owned()),
    );

    assert_eq!(status.eligibility, Eligibility::Eligible);
    assert_eq!(status.revision, Some(RevisionStatus::Current));
}

#[test]
fn compares_only_known_eligible_revisions() {
    let current = assess_mcu(
        &mcu(Some("Kalico"), Some("v1"), "CONFIG=x\n"),
        &CheckoutRevision::Known("v1".to_owned()),
    );
    assert_eq!(current.revision, Some(RevisionStatus::Current));
    let outdated = assess_mcu(
        &mcu(Some("Klipper"), Some("v0"), "CONFIG=x\n"),
        &CheckoutRevision::Known("v1".to_owned()),
    );
    assert_eq!(outdated.revision, Some(RevisionStatus::UpdateRequired));
    let unknown = assess_mcu(
        &mcu(Some("Klipper"), None, "CONFIG=x\n"),
        &CheckoutRevision::Indeterminate,
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
        &CheckoutRevision::Known("v0.13.0-762-g9871eeef".to_owned()),
    );
    assert_eq!(status.revision, Some(RevisionStatus::Current));
}

#[test]
fn selects_only_outdated_eligible_mcus_unless_explicitly_overridden() {
    let current = mcu(Some("Klipper"), Some("v1"), "CONFIG=x\n");
    let outdated = mcu(Some("Klipper"), Some("v0"), "CONFIG=x\n");
    let external = mcu(Some("Beacon"), Some("v0"), "CONFIG=x\n");
    let checkout = CheckoutRevision::Known("v1".to_owned());
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
    assert!(is_selected(
        &current,
        &checkout,
        &UpdateSelection::Force("mcu".to_owned())
    ));
    assert!(!is_selected(&external, &checkout, &UpdateSelection::All));
}
