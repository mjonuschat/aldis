//! Read-only MCU update eligibility and revision comparison.

use crate::moonraker::Mcu;

/// Whether this updater may manage a discovered MCU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Eligibility {
    /// A Klipper-family MCU with the embedded configuration required to build it.
    Eligible,
    /// Firmware managed by another project, named as Moonraker reported it (e.g. "Beacon").
    ExternallyManaged(String),
    /// Legacy or unidentified Klipper-family firmware without required updater metadata.
    Unsupported(UnsupportedReason),
}

/// The Klipper-family firmware a version requirement applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareKind {
    Klipper,
    Kalico,
}

impl std::fmt::Display for FirmwareKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FirmwareKind::Klipper => "Klipper",
            FirmwareKind::Kalico => "Kalico",
        })
    }
}

/// Why a Klipper-family MCU is not eligible for management.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UnsupportedReason {
    /// The MCU is recent enough but reported no embedded Kconfig.
    #[error("no embedded configuration found")]
    NoEmbeddedConfig,
    /// The MCU's running firmware predates the minimum version required to report `mcu_kconfig`.
    #[error("{firmware} version too old ({running} < {minimum})")]
    VersionTooOld {
        firmware: FirmwareKind,
        running: String,
        minimum: &'static str,
    },
}

/// Minimum Klipper version required to reliably report `mcu_kconfig`; see README.md.
const KLIPPER_MINIMUM_VERSION: &str = "v0.13.0-753-g8c29c0a8e";
/// Minimum Kalico version required to reliably report `mcu_kconfig`; see README.md.
const KALICO_MINIMUM_VERSION: &str = "v2026.09.00-3-g6127720c4";

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
/// `selected` and `running` (Klipper's own build) are computed by separate `git
/// describe` invocations, possibly with different git versions, so an exact
/// string match would spuriously report an up-to-date MCU as needing an update.
/// Any abbreviation of the same commit hash is a prefix of any longer one, so
/// tolerate a length difference in the hash by comparing prefixes instead.
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
    // Real Klipper and Kalico always populate `app`; third-party firmware such as Beacon
    // reports it as an empty string instead of omitting it, so treat blank the same as absent.
    match mcu
        .app
        .as_deref()
        .map(str::trim)
        .filter(|app| !app.is_empty())
    {
        Some(app) if app.eq_ignore_ascii_case("klipper") => {
            classify_known_firmware(mcu, FirmwareKind::Klipper, KLIPPER_MINIMUM_VERSION)
        }
        Some(app) if app.eq_ignore_ascii_case("kalico") => {
            classify_known_firmware(mcu, FirmwareKind::Kalico, KALICO_MINIMUM_VERSION)
        }
        Some(app) => Eligibility::ExternallyManaged(app.to_owned()),
        None => classify_unidentified(mcu),
    }
}

/// Classifies an MCU that reported no usable `app` name.
///
/// Firmware that doesn't identify itself via `app` (e.g. Beacon) still names itself as the
/// leading word of `mcu_version` (e.g. `"Beacon 2.1.0"`), unlike Klipper/Kalico's own
/// `git describe`-shaped version (`vMAJOR.MINOR.PATCH-COMMITS-gHASH`). Use that name when
/// present; otherwise fall back to the embedded-config check used for genuinely unidentified
/// legacy firmware.
fn classify_unidentified(mcu: &Mcu) -> Eligibility {
    if let Some(version) = mcu.version.as_deref()
        && !looks_like_git_describe(version)
        && let Some(name) = version.split_whitespace().next()
    {
        return Eligibility::ExternallyManaged(name.to_owned());
    }
    classify_by_embedded_config(mcu)
}

fn looks_like_git_describe(version: &str) -> bool {
    version
        .strip_prefix(['v', 'V'])
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

fn classify_known_firmware(
    mcu: &Mcu,
    firmware: FirmwareKind,
    minimum_version: &'static str,
) -> Eligibility {
    if let Some(running) = mcu.version.as_deref()
        && let Some(running_parsed) = parse_git_describe(running)
        && let Some(minimum_parsed) = parse_git_describe(minimum_version)
        && running_parsed < minimum_parsed
    {
        return Eligibility::Unsupported(UnsupportedReason::VersionTooOld {
            firmware,
            running: running.to_owned(),
            minimum: minimum_version,
        });
    }
    classify_by_embedded_config(mcu)
}

fn classify_by_embedded_config(mcu: &Mcu) -> Eligibility {
    if mcu.kconfig.trim().is_empty() {
        Eligibility::Unsupported(UnsupportedReason::NoEmbeddedConfig)
    } else {
        Eligibility::Eligible
    }
}

/// A parsed `git describe --tags --long` version: `vMAJOR.MINOR.PATCH[-COMMITS[-gHASH]]`.
///
/// Ordered by `(major, minor, patch, commits)` so a version on a newer tag always outranks
/// one on an older tag regardless of commit count, matching how the tags themselves are cut.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct ParsedVersion {
    major: u32,
    minor: u32,
    patch: u32,
    commits: u32,
}

fn parse_git_describe(version: &str) -> Option<ParsedVersion> {
    let version = version.strip_prefix('v')?;
    let mut segments = version.splitn(3, '-');
    let core = segments.next()?;
    let commits = segments.next().unwrap_or("0");

    let mut core_parts = core.splitn(3, '.');
    let major = core_parts.next()?.parse().ok()?;
    let minor = core_parts.next()?.parse().ok()?;
    let patch = core_parts.next()?.parse().ok()?;
    let commits = commits.parse().ok()?;

    Some(ParsedVersion {
        major,
        minor,
        patch,
        commits,
    })
}
