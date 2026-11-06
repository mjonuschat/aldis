//! Read-only MCU update eligibility and revision comparison.

use crate::moonraker::Mcu;
use crate::prebuilt::{self, Running};

/// Whether this updater may manage a discovered MCU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Eligibility {
    /// A Klipper-family MCU with the embedded configuration required to build it.
    Eligible,
    /// Firmware not built from the Klipper checkout that this updater can replace with a
    /// prebuilt image: a FOCI release build, a Beacon, or a Cartographer probe.
    Prebuilt(Running),
    /// Firmware managed by another project, named as Moonraker reported it (e.g. "Beacon").
    ExternallyManaged(String),
    /// Legacy or unidentified Klipper-family firmware without required updater metadata.
    Unsupported(UnsupportedReason),
    /// A Klipper-family MCU whose family has no bootloader this updater can drive, named as
    /// the embedded configuration reports it (e.g. "ATSAMD").
    UnsupportedMcu(String),
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
    /// Prebuilt firmware no update image can be matched to.
    #[error(transparent)]
    Prebuilt(prebuilt::Unsupported),
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
    /// The running firmware is older than the checkout or image it would be updated to.
    UpdateRequired,
    /// The running firmware is newer than its target: nothing to update, and an update never
    /// replaces it with the older one.
    Newer,
    /// One or both revisions cannot safely be compared.
    Indeterminate,
}

/// How a running version compares with the version it would be updated to: older is an update,
/// equal is current, and newer needs none.
pub fn compare_versions<T: Ord>(running: &T, target: &T) -> RevisionStatus {
    match running.cmp(target) {
        std::cmp::Ordering::Less => RevisionStatus::UpdateRequired,
        std::cmp::Ordering::Equal => RevisionStatus::Current,
        std::cmp::Ordering::Greater => RevisionStatus::Newer,
    }
}

/// A selected Klipper checkout revision suitable for comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckoutRevision {
    /// A clean, comparable checkout revision.
    Known(String),
    /// The checkout cannot safely be compared to a running MCU.
    Indeterminate,
}

impl CheckoutRevision {
    /// The revision for display, `unknown` when indeterminate.
    pub fn label(&self) -> &str {
        match self {
            Self::Known(revision) => revision,
            Self::Indeterminate => "unknown",
        }
    }
}

/// What each MCU is compared against and updated to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetVersions {
    /// The Klipper checkout revision Klipper-family MCUs are built from.
    pub klipper: CheckoutRevision,
    /// What the prebuilt-firmware MCUs (FOCI, Beacon, Cartographer) are updated to.
    pub prebuilt: prebuilt::Targets,
}

impl TargetVersions {
    /// Targets for Klipper-family MCUs built from `checkout`, and no known prebuilt targets.
    pub fn klipper(checkout: CheckoutRevision) -> Self {
        Self {
            klipper: checkout,
            prebuilt: prebuilt::Targets::default(),
        }
    }

    /// These targets with the prebuilt-firmware MCUs updated to `targets`.
    pub fn with_prebuilt(mut self, targets: prebuilt::Targets) -> Self {
        self.prebuilt = targets;
        self
    }

    /// These targets with FOCI MCUs updated to `release`.
    pub fn with_foci_release(mut self, release: Option<crate::foci::Release>) -> Self {
        self.prebuilt.foci = release;
        self
    }

    /// These targets with each Beacon updated to its image in `targets`.
    pub fn with_beacon_targets(mut self, targets: crate::beacon::Targets) -> Self {
        self.prebuilt.beacon = targets;
        self
    }

    /// These targets with each Cartographer updated to its image in `targets`.
    pub fn with_cartographer_targets(mut self, targets: crate::cartographer::Targets) -> Self {
        self.prebuilt.cartographer = targets;
        self
    }

    /// Targets nothing is compared against, for a run that only verifies reconnection.
    pub fn indeterminate() -> Self {
        Self::klipper(CheckoutRevision::Indeterminate)
    }

    /// The target `mcu` is updated to, for display; `unknown` when indeterminate.
    pub fn label_for(&self, mcu: &Mcu) -> String {
        self.prebuilt
            .label_for(mcu)
            .unwrap_or_else(|| self.klipper.label().to_owned())
    }

    /// Whether `mcu` runs its target: `None` when there is no known target to check against.
    pub fn reached_by(&self, mcu: &Mcu) -> Option<bool> {
        if let Some(reached) = self.prebuilt.reached_by(mcu) {
            return reached;
        }
        match &self.klipper {
            CheckoutRevision::Known(revision) => Some(
                mcu.version
                    .as_deref()
                    .is_some_and(|version| revisions_match(version, revision)),
            ),
            CheckoutRevision::Indeterminate => None,
        }
    }
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
    /// Offer every eligible MCU regardless of revision state (`--all`, or `--force` on the
    /// MCUs the caller already narrowed the inventory to).
    All,
}

/// How an MCU's running Klipper revision compares with the checkout's it would be built from.
/// The same commit is current, whatever the length of its hash. Otherwise an older one is an
/// update, a newer one needs none and is not replaced by an update, and a different commit at
/// the same position, or a revision with no tag, is an update.
fn compare_revisions(running: &str, selected: &str) -> RevisionStatus {
    if revisions_match(running, selected) {
        return RevisionStatus::Current;
    }
    match (klipper_version(running), klipper_version(selected)) {
        (Some(running), Some(selected)) => match compare_versions(&running, &selected) {
            RevisionStatus::Current => RevisionStatus::UpdateRequired,
            status => status,
        },
        _ => RevisionStatus::UpdateRequired,
    }
}

/// An update that would replace newer firmware with an older build or image.
pub struct Downgrade {
    /// What the MCU would be updated to, for "newer than ...".
    pub description: String,
    /// What to do instead.
    pub advice: String,
}

/// What `mcu` would be downgraded to by an update, when its target is older than what it runs.
/// This is what [`RevisionStatus::Newer`] means.
fn downgrade_of(mcu: &Mcu, versions: &TargetVersions) -> Option<Downgrade> {
    match classify_mcu(mcu) {
        Eligibility::Eligible => match (&mcu.version, &versions.klipper) {
            (Some(running), CheckoutRevision::Known(selected))
                if compare_revisions(running, selected) == RevisionStatus::Newer =>
            {
                Some(Downgrade {
                    description: format!("the {} checkout", selected),
                    advice: "update the checkout first".to_owned(),
                })
            }
            _ => None,
        },
        Eligibility::Prebuilt(running) => versions.prebuilt.downgrade(&running, &mcu.name),
        _ => None,
    }
}

/// What `mcu` runs newer firmware than, for saying so: the checkout, image or release it would
/// otherwise be updated to.
pub fn newer_than(mcu: &Mcu, versions: &TargetVersions) -> Option<String> {
    downgrade_of(mcu, versions).map(|downgrade| downgrade.description)
}

/// Why an update will not replace `mcu`'s firmware with the build or image it would otherwise
/// get, when it is because that would be a downgrade. No update downgrades firmware, whatever
/// the selection.
pub fn downgrade_refusal(mcu: &Mcu, versions: &TargetVersions) -> Option<String> {
    let downgrade = downgrade_of(mcu, versions)?;
    Some(format!(
        "{} runs {}, newer than {}, and an update never downgrades it: {}",
        mcu.name,
        mcu.version.as_deref().unwrap_or("unknown"),
        downgrade.description,
        downgrade.advice
    ))
}

/// Returns whether one MCU is eligible for the requested update selection.
pub fn is_selected(mcu: &Mcu, versions: &TargetVersions, selection: &UpdateSelection) -> bool {
    let status = assess_mcu(mcu, versions);
    let updatable = match status.eligibility {
        Eligibility::Eligible => true,
        // Prebuilt firmware is flashed with its target image, so even `All` needs one to be
        // known: the FOCI release, or the Beacon's or Cartographer's image from its checkout.
        Eligibility::Prebuilt(ref running) => {
            versions.prebuilt.target(running, &mcu.name).is_some()
        }
        _ => false,
    };
    // No selection downgrades firmware.
    if !updatable || downgrade_of(mcu, versions).is_some() {
        return false;
    }
    match selection {
        UpdateSelection::Required => status.revision == Some(RevisionStatus::UpdateRequired),
        UpdateSelection::All => true,
    }
}

/// Classifies one MCU and compares its revision when eligible.
pub fn assess_mcu(mcu: &Mcu, versions: &TargetVersions) -> McuStatus {
    let eligibility = classify_mcu(mcu);
    let revision = match &eligibility {
        Eligibility::Eligible => Some(match (&mcu.version, &versions.klipper) {
            (Some(running), CheckoutRevision::Known(selected)) => {
                compare_revisions(running, selected)
            }
            _ => RevisionStatus::Indeterminate,
        }),
        Eligibility::Prebuilt(running) => Some(versions.prebuilt.compare(running, &mcu.name)),
        _ => None,
    };
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
    match prebuilt::identify(mcu) {
        Some(Ok(running)) => return Eligibility::Prebuilt(running),
        Some(Err(reason)) => {
            return Eligibility::Unsupported(UnsupportedReason::Prebuilt(reason));
        }
        None => {}
    }
    // Real Klipper and Kalico always populate `app`; third-party firmware such as Beacon
    // reports it as an empty string instead of omitting it, so treat blank the same as absent.
    match mcu.app_name() {
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
        && let Some(name) = mcu.version_name()
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
        && let Some(running_parsed) = klipper_version(running)
        && let Some(minimum_parsed) = klipper_version(minimum_version)
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

/// Families with a bootloader this updater can drive: Katapult builds only for STM32,
/// RP2040/RP235x, and LPC176x; STM32 and RP parts also carry ROM DFU and PicoBoot; the Linux
/// host MCU is installed as a process.
const FLASHABLE_FAMILIES: &[&str] = &["STM32", "RPXXXX", "RP2040", "LPC176X", "LINUX"];

/// The first enabled `CONFIG_MACH_*=y` line of an embedded configuration, or an empty string.
pub fn kconfig_machine(kconfig: &str) -> String {
    kconfig
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("CONFIG_MACH_") && line.ends_with("=y"))
        .unwrap_or_default()
        .to_owned()
}

enum FamilySupport {
    Flashable,
    Unflashable(String),
    Unstated,
}

fn family_support(kconfig: &str) -> FamilySupport {
    let families: Vec<&str> = kconfig
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("CONFIG_MACH_")?.strip_suffix("=y"))
        .collect();
    if families.iter().any(|family| {
        FLASHABLE_FAMILIES.contains(family) || crate::flash::samba::is_flashable_part(family)
    }) {
        FamilySupport::Flashable
    } else if let Some(family) = families.first() {
        FamilySupport::Unflashable((*family).to_owned())
    } else {
        FamilySupport::Unstated
    }
}

fn classify_by_embedded_config(mcu: &Mcu) -> Eligibility {
    if mcu.kconfig.trim().is_empty() {
        return Eligibility::Unsupported(UnsupportedReason::NoEmbeddedConfig);
    }
    match family_support(&mcu.kconfig) {
        FamilySupport::Unflashable(family) => Eligibility::UnsupportedMcu(family),
        FamilySupport::Flashable | FamilySupport::Unstated => Eligibility::Eligible,
    }
}

/// A Klipper or Kalico revision, `git describe --tags --long`'s `vMAJOR.MINOR.PATCH[-COMMITS[-gHASH]]`,
/// as a semver version that orders like the history it names: the tags themselves, and then the
/// commits since the latest one. Commits after a tag are work towards the next release, so
/// `v0.13.0-756-g2d7717e3` is `0.13.1-dev.756`, newer than `v0.13.0` and older than `v0.13.1`.
/// The hash and the dirty marker carry no order and are left out, so equal versions may still be
/// different commits (see [`revisions_match`]). `None` when the revision names no tag.
///
/// Counting commits orders one branch's history only. Versions from different branches or forks
/// compare by their counts, which says nothing about which has the other's changes.
pub fn klipper_version(revision: &str) -> Option<semver::Version> {
    let revision = revision.strip_prefix('v')?;
    let mut segments = revision.splitn(3, '-');
    let core = segments.next()?;
    let commits: u64 = segments.next().unwrap_or("0").parse().ok()?;
    let mut core = core.splitn(3, '.');
    let major: u64 = core.next()?.parse().ok()?;
    let minor: u64 = core.next()?.parse().ok()?;
    let patch: u64 = core.next()?.parse().ok()?;
    if commits == 0 {
        return Some(semver::Version::new(major, minor, patch));
    }
    let mut version = semver::Version::new(major, minor, patch + 1);
    version.pre = semver::Prerelease::new(&format!("dev.{commits}")).ok()?;
    Some(version)
}
