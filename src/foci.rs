//! FOCI firmware: which published release build a running MCU corresponds to.
//!
//! [FOCI](https://github.com/foci-rs/foci) is Rust firmware for TMC4671 servo boards that speaks
//! Klipper's MCU protocol. It reports no Kconfig and is not built from the Klipper checkout, so
//! aldis cannot build it. It can instead replace a release build with the same board's and
//! flavor's build from a newer FOCI release.

use std::cmp::Ordering;

use crate::eligibility::RevisionStatus;
use crate::moonraker::Mcu;

/// The `app` FOCI reports in its identify dictionary.
pub const APP: &str = "FOCI";

/// A board FOCI publishes release builds for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Board {
    /// Ouroboros, an STM32H723 board.
    Ouroboros,
    /// OpenFFBoard, an STM32F407 board.
    OpenFfboard,
}

impl Board {
    /// The board whose firmware reports `mcu` as its `MCU` constant. Each board has its own MCU.
    pub fn from_mcu(mcu: &str) -> Option<Self> {
        match mcu {
            "stm32h723xx" => Some(Self::Ouroboros),
            "stm32f407xx" => Some(Self::OpenFfboard),
            _ => None,
        }
    }

    /// The board's firmware crate, which names its release assets.
    pub fn crate_name(self) -> &'static str {
        match self {
            Self::Ouroboros => "ouroboros-fw",
            Self::OpenFfboard => "openffboard-fw",
        }
    }

    /// Where release builds are linked to start (the board's `memory.x`), just above the
    /// Katapult bootloader FOCI documents for it: 128 KiB on Ouroboros, 32 KiB on OpenFFBoard.
    pub fn application_start(self) -> u32 {
        match self {
            Self::Ouroboros => 0x0802_0000,
            Self::OpenFfboard => 0x0800_8000,
        }
    }
}

/// A release build flavor, told apart by the cargo features `build_versions` lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flavor {
    /// The production build, built without features (`features=none`).
    Prod,
    /// The tracing build, built with `--features trace` (`features=trace`).
    Trace,
}

impl Flavor {
    /// The flavor as release asset names spell it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Trace => "trace",
        }
    }

    /// The flavor whose features `build_versions` lists, e.g.
    /// `rust=1.98.1 features=none board=a82de21d ...`.
    pub fn from_build_versions(build_versions: &str) -> Result<Self, Unsupported> {
        let features = build_versions
            .split_whitespace()
            .find_map(|field| field.strip_prefix("features="))
            .ok_or(Unsupported::NoBuildFeatures)?;
        match features {
            "none" => Ok(Self::Prod),
            "trace" => Ok(Self::Trace),
            other => Err(Unsupported::UnknownFlavor(other.to_owned())),
        }
    }
}

/// A FOCI version: `v<package>-<commits since the version bump>-g<board commit>[-dirty]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    /// The firmware crate's package version.
    pub package: semver::Version,
    /// Commits on the board repository since its package version was last changed.
    pub commits: u32,
    /// Whether the board repository had uncommitted changes.
    pub dirty: bool,
}

impl Version {
    /// Parses a version as FOCI's `build.rs` assembles it. The package version may carry a
    /// pre-release (`v0.4.0-rc.1-0-g0fb68e1b`), so the commit count is the first all-digit field
    /// that follows a valid package version and precedes the `g` commit field.
    pub fn parse(version: &str) -> Option<Self> {
        let version = version.strip_prefix('v')?;
        let (version, dirty) = version
            .strip_suffix("-dirty")
            .map_or((version, false), |base| (base, true));
        let fields: Vec<&str> = version.split('-').collect();
        (1..fields.len().saturating_sub(1)).find_map(|index| {
            let commits = fields[index];
            if commits.is_empty() || !commits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            fields[index + 1]
                .strip_prefix('g')
                .filter(|hash| !hash.is_empty())?;
            let package = semver::Version::parse(&fields[..index].join("-")).ok()?;
            Some(Self {
                package,
                commits: commits.parse().ok()?,
                dirty,
            })
        })
    }

    /// Whether this is how a release build reports itself: the release workflow builds each
    /// board at its version-bump commit, clean.
    pub fn is_release_build(&self) -> bool {
        self.commits == 0 && !self.dirty
    }
}

/// A running FOCI build, matched to the release assets that could replace it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Firmware {
    pub board: Board,
    pub flavor: Flavor,
    pub version: Version,
}

/// Why a FOCI MCU cannot be matched to a release asset.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unsupported {
    /// The `MCU` constant names a board FOCI publishes no release for.
    #[error("FOCI publishes no release builds for {0} boards")]
    UnknownBoard(String),
    /// `build_versions` lists features no release flavor is built with.
    #[error("FOCI build features {0:?} match no release flavor")]
    UnknownFlavor(String),
    /// `build_versions` was missing or listed no features.
    #[error("FOCI did not report its build features")]
    NoBuildFeatures,
    /// The version is not shaped the way FOCI's build reports it.
    #[error("unrecognized FOCI version {0:?}")]
    UnparsableVersion(String),
}

/// Whether `mcu` runs FOCI.
pub fn is_foci(mcu: &Mcu) -> bool {
    mcu.app_name()
        .is_some_and(|app| app.eq_ignore_ascii_case(APP))
}

/// Matches a FOCI MCU's identify data to a board, flavor and version.
pub fn identify(mcu: &Mcu) -> Result<Firmware, Unsupported> {
    let board =
        Board::from_mcu(&mcu.mcu).ok_or_else(|| Unsupported::UnknownBoard(mcu.mcu.clone()))?;
    let flavor = Flavor::from_build_versions(
        mcu.build_versions
            .as_deref()
            .ok_or(Unsupported::NoBuildFeatures)?,
    )?;
    let running = mcu.version.as_deref().unwrap_or_default();
    let version = Version::parse(running)
        .ok_or_else(|| Unsupported::UnparsableVersion(running.to_owned()))?;
    Ok(Firmware {
        board,
        flavor,
        version,
    })
}

/// A published FOCI release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Release {
    /// The release's git tag, e.g. `v0.4.1`.
    pub tag: String,
    /// The version the tag names.
    pub version: semver::Version,
}

impl Release {
    /// The release a `v<semver>` tag names.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let version = semver::Version::parse(tag.strip_prefix('v')?).ok()?;
        Some(Self {
            tag: tag.to_owned(),
            version,
        })
    }

    /// The release's firmware image for one board and flavor, as the release workflow names it:
    /// `<crate>-v<version>-<flavor>.bin`.
    pub fn asset_name(&self, board: Board, flavor: Flavor) -> String {
        format!(
            "{}-v{}-{}.bin",
            board.crate_name(),
            self.version,
            flavor.label()
        )
    }
}

/// How a running build compares with `release`. Only release builds are compared: a local or
/// development build cannot be placed relative to a release, and neither can one newer than it.
pub fn compare(running: &Version, release: &Release) -> RevisionStatus {
    if !running.is_release_build() {
        return RevisionStatus::Indeterminate;
    }
    match running.package.cmp(&release.version) {
        Ordering::Equal => RevisionStatus::Current,
        Ordering::Less => RevisionStatus::UpdateRequired,
        Ordering::Greater => RevisionStatus::Indeterminate,
    }
}
