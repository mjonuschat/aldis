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

/// Policy controlling which eligible MCUs may be offered for update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateSelection {
    /// Offer only known out-of-date MCUs.
    Required,
    /// Offer one named eligible MCU regardless of revision state.
    Force(String),
    /// Offer every eligible MCU regardless of revision state.
    All,
}

/// Returns whether one MCU is eligible for the requested update selection.
pub fn is_selected(mcu: &Mcu, checkout: &CheckoutRevision, selection: &UpdateSelection) -> bool {
    let status = assess_mcu(mcu, checkout);
    if status.eligibility != Eligibility::Eligible {
        return false;
    }
    match selection {
        UpdateSelection::Required => status.revision == Some(RevisionStatus::UpdateRequired),
        UpdateSelection::Force(name) => mcu.name == *name,
        UpdateSelection::All => true,
    }
}

/// Classifies one MCU and compares its revision when eligible.
pub fn assess_mcu(mcu: &Mcu, checkout: &CheckoutRevision) -> McuStatus {
    let eligibility = classify_mcu(mcu);
    let revision = (eligibility == Eligibility::Eligible).then(|| match (&mcu.version, checkout) {
        (Some(running), CheckoutRevision::Known(selected))
            if revisions_match(running, selected) =>
        {
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

/// Compares a running firmware version against a selected checkout revision.
///
/// The two sides may embed the commit hash at different abbreviation lengths:
/// libgit2 (used to compute `selected`) has no equivalent to real git's size-based
/// "auto" abbreviation heuristic (used by Klipper's own build to compute `running`),
/// so an exact string match would spuriously report an up-to-date MCU as needing an
/// update. Any abbreviation of the same commit hash is a prefix of any longer one,
/// so tolerate a length difference in the hash by comparing prefixes instead.
pub fn revisions_match(running: &str, selected: &str) -> bool {
    let (running_base, running_dirty) = split_dirty_suffix(running);
    let (selected_base, selected_dirty) = split_dirty_suffix(selected);
    if running_dirty != selected_dirty {
        return false;
    }
    if running_base.len() <= selected_base.len() {
        selected_base.starts_with(running_base)
    } else {
        running_base.starts_with(selected_base)
    }
}

fn split_dirty_suffix(revision: &str) -> (&str, bool) {
    if let Some(base) = strip_dirty_build_suffix(revision) {
        return (base, true);
    }
    revision
        .strip_suffix("-dirty")
        .map_or((revision, false), |base| (base, true))
}

/// Strips Klipper's full dirty-build suffix as `buildcommands.py` embeds it
/// in an MCU's reported version: `"%s-%s-%s" % (version, btime, hostname)`,
/// where `version` already ends `-dirty` and `btime` is
/// `time.strftime("%Y%m%d_%H%M%S")`. The hostname may itself contain
/// hyphens, so only the fixed-shape `-dirty-<15-char timestamp>-` prefix is
/// matched; everything after it is accepted as the hostname.
fn strip_dirty_build_suffix(revision: &str) -> Option<&str> {
    let (base, rest) = revision.split_once("-dirty-")?;
    let (timestamp, hostname) = rest.split_once('-')?;
    (is_klipper_build_timestamp(timestamp) && !hostname.is_empty()).then_some(base)
}

fn is_klipper_build_timestamp(candidate: &str) -> bool {
    candidate.len() == 15
        && candidate.as_bytes()[8] == b'_'
        && candidate[..8].bytes().all(|byte| byte.is_ascii_digit())
        && candidate[9..].bytes().all(|byte| byte.is_ascii_digit())
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
        None => {
            if mcu.kconfig.trim().is_empty() {
                Eligibility::Unsupported
            } else {
                Eligibility::Eligible
            }
        }
    }
}
