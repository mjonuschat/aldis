use aldis::eligibility::{
    CheckoutRevision, Eligibility, RevisionStatus, UpdateSelection, assess_mcu, is_selected,
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
        Eligibility::ExternallyManaged
    );
    assert_eq!(
        assess_mcu(
            &mcu(None, Some("v1"), ""),
            &CheckoutRevision::Known("v1".to_owned())
        )
        .eligibility,
        Eligibility::Unsupported
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
