//! Read-only MCU update eligibility and revision comparison.

use crate::moonraker::Mcu;

/// Whether this updater may manage a discovered MCU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Eligibility {
    /// A Klipper-family MCU with the embedded configuration required to build it.
    Eligible,
    /// Firmware managed by another project.
    ExternallyManaged,
    /// Legacy or unidentified firmware without required updater metadata.
    Unsupported,
}

/// The comparison result for one eligible MCU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionStatus {
    /// The running firmware revision equals the selected checkout revision.
    Current,
    /// The known running and checkout revisions differ.
    UpdateRequired,
    /// One or both revisions cannot safely be compared.
    Indeterminate,
}

/// A selected Klipper checkout revision suitable for comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckoutRevision {
    /// A clean, comparable checkout revision.
    Known(String),
    /// The checkout cannot safely be compared to a running MCU.
    Indeterminate,
}

/// Read-only eligibility and revision result for one MCU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McuStatus {
    /// Whether the updater may manage this MCU.
    pub eligibility: Eligibility,
    /// Revision state when the MCU is eligible.
    pub revision: Option<RevisionStatus>,
}

/// Classifies one MCU and compares its revision when eligible.
pub fn assess_mcu(mcu: &Mcu, checkout: &CheckoutRevision) -> McuStatus {
    let eligibility = classify_mcu(mcu);
    let revision = (eligibility == Eligibility::Eligible).then(|| match (&mcu.version, checkout) {
        (Some(running), CheckoutRevision::Known(selected)) if running == selected => {
            RevisionStatus::Current
        }
        (Some(_), CheckoutRevision::Known(_)) => RevisionStatus::UpdateRequired,
        _ => RevisionStatus::Indeterminate,
    });
    McuStatus {
        eligibility,
        revision,
    }
}

/// Classifies eligibility without inferring from MCU family or transport.
pub fn classify_mcu(mcu: &Mcu) -> Eligibility {
    match mcu.app.as_deref() {
        Some(app) if app.eq_ignore_ascii_case("klipper") || app.eq_ignore_ascii_case("kalico") => {
            if mcu.kconfig.trim().is_empty() {
                Eligibility::Unsupported
            } else {
                Eligibility::Eligible
            }
        }
        Some(_) => Eligibility::ExternallyManaged,
        None => Eligibility::Unsupported,
    }
}
