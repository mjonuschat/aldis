use mcu_update::eligibility::{CheckoutRevision, Eligibility, RevisionStatus, assess_mcu};
use mcu_update::moonraker::Mcu;

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
fn classifies_supported_external_and_legacy_firmware_without_inference() {
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
