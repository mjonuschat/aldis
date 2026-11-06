//! Cartographer firmware: which image from the user's `cartographer_firmware` clone a running
//! Cartographer probe takes.
//!
//! [Cartographer](https://github.com/Cartographer3D/cartographer_firmware) probes speak Klipper's
//! MCU protocol, but their firmware is published only as prebuilt images, one per probe
//! generation, link (USB, or CAN at one bitrate) and build (full or lite), listed in the clone's
//! `firmware_list.csv`. The firmware never checks that an image suits the board, and the wrong
//! link, bitrate or generation leaves the probe recoverable only through its ROM bootloader with
//! its boot pins bridged by hand. So the manifest only nominates candidates: each image is matched
//! to the probe by the identify dictionary embedded in the image itself, which names its version,
//! build, MCU and CAN bitrate exactly as the probe will report them once it runs.
//!
//! Cartographer's plugin removes every scan and touch model calibrated under another firmware
//! version, so every update means recalibrating the probe, which the user is told.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::eligibility::{RevisionStatus, compare_versions};
use crate::identify::DictionaryIdentity;
use crate::moonraker::{Mcu, McuInventory, McuTransport};

/// The `app` Cartographer firmware reports from 6.1.0 on; older builds report none and name
/// themselves only in their version.
pub const APP: &str = "Cartographer";

/// Where Cartographer's Katapult starts applications: 8 KiB into flash.
pub const APPLICATION_START: u32 = 0x0800_2000;

/// A probe generation, as the manifest's `probe_version` names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Generation {
    /// V2 and V3 probes, on an STM32F042.
    V3,
    /// V4 (and HT) probes, on an STM32G431 or STM32G474.
    V4,
}

impl Generation {
    /// The generation whose firmware reports `mcu` as its `MCU` constant.
    pub fn from_mcu(mcu: &str) -> Option<Self> {
        match mcu {
            "stm32f042x6" => Some(Self::V3),
            "stm32g431xx" | "stm32g474xx" => Some(Self::V4),
            _ => None,
        }
    }

    /// The generation as the manifest's `probe_version` column names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::V3 => "v3",
            Self::V4 => "v4",
        }
    }
}

/// A firmware build: the full one, or the lite one for slow hosts (which the 5.x V3 releases
/// called the K1 build).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Build {
    Full,
    Lite,
}

impl Build {
    /// The build a reported version names: `CARTOGRAPHER V4 6.1.0 lite` and
    /// `CARTOGRAPHER K1 5.1.0` are lite, `CARTOGRAPHER V4 6.1.0` is full.
    pub fn from_version(version: &str) -> Self {
        if version
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case("lite") || word.eq_ignore_ascii_case("k1"))
        {
            Self::Lite
        } else {
            Self::Full
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lite => "lite",
        }
    }
}

/// The `major.minor.patch` a reported version holds, e.g. 6.1.0 in `CARTOGRAPHER V4 6.1.0`. A
/// version with no such number, or more than one, has none.
pub fn parse_version(version: &str) -> Option<semver::Version> {
    let mut numbers = version.split_whitespace().filter_map(|word| {
        let parts: Vec<&str> = word.split('.').collect();
        (parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())))
        .then(|| semver::Version::parse(word).ok())
        .flatten()
    });
    let version = numbers.next()?;
    numbers.next().is_none().then_some(version)
}

/// A running Cartographer probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Firmware {
    pub generation: Generation,
    pub build: Build,
    pub version: semver::Version,
    /// The `MCU` constant the firmware reports.
    pub mcu: String,
    /// The CAN bitrate a CAN build reports; `None` for a USB build.
    pub can_frequency: Option<u64>,
}

/// Why a Cartographer probe cannot be matched to an image.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unsupported {
    #[error("Cartographer firmware for {0} probes is not published as an update image")]
    UnknownProbe(String),
    #[error("Cartographer version {0:?} names no version number")]
    UnparsableVersion(String),
}

/// Whether `mcu` runs Cartographer firmware: it reports `app` `Cartographer`, or, before 6.1.0,
/// an empty one and a version starting with `Cartographer`.
pub fn is_cartographer(mcu: &Mcu) -> bool {
    names_itself_cartographer(
        mcu.app.as_deref(),
        mcu.version.as_deref().unwrap_or_default(),
    )
}

/// Whether firmware reporting `app` and `version`, whether running or embedded in an image, names
/// itself Cartographer: by its `app`, or, with a blank one, by a version starting with it.
fn names_itself_cartographer(app: Option<&str>, version: &str) -> bool {
    match app.map(str::trim).filter(|app| !app.is_empty()) {
        Some(app) => app.eq_ignore_ascii_case(APP),
        None => version
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case(APP)),
    }
}

/// Matches a Cartographer MCU's identify data to a generation, build and version; `None` when
/// `mcu` is not a Cartographer.
pub fn identify(mcu: &Mcu) -> Option<Result<Firmware, Unsupported>> {
    if !is_cartographer(mcu) {
        return None;
    }
    Some(identify_cartographer(mcu))
}

fn identify_cartographer(mcu: &Mcu) -> Result<Firmware, Unsupported> {
    let generation =
        Generation::from_mcu(&mcu.mcu).ok_or_else(|| Unsupported::UnknownProbe(mcu.mcu.clone()))?;
    let reported = mcu.version.as_deref().unwrap_or_default();
    let version = parse_version(reported)
        .ok_or_else(|| Unsupported::UnparsableVersion(reported.to_owned()))?;
    Ok(Firmware {
        generation,
        build: Build::from_version(reported),
        version,
        mcu: mcu.mcu.clone(),
        can_frequency: mcu.canbus_frequency_hz,
    })
}

/// How a running probe compares with the image it would be updated to. A probe newer than the
/// image is not downgraded unless asked to be.
pub fn compare(running: &Firmware, target: &Target) -> RevisionStatus {
    compare_versions(&running.version, &target.version)
}

/// What a firmware image says about itself in its embedded identify dictionary: what the probe
/// will report once it runs the image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageIdentity {
    pub version: String,
    pub app: Option<String>,
    pub mcu: String,
    pub can_frequency: Option<u64>,
}

/// The most a decompressed identify dictionary may hold.
const DICTIONARY_LIMIT: u64 = 1024 * 1024;

/// Reads the identify dictionary Klipper's build embeds, zlib-compressed, in a firmware image.
pub fn image_identity(image: &[u8]) -> Option<ImageIdentity> {
    (0..image.len().saturating_sub(2)).find_map(|offset| {
        let (cmf, flg) = (image[offset], image[offset + 1]);
        if cmf != 0x78 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
            return None;
        }
        let mut text = Vec::new();
        flate2::read::ZlibDecoder::new(&image[offset..])
            .take(DICTIONARY_LIMIT)
            .read_to_end(&mut text)
            .ok()?;
        let identity = DictionaryIdentity::from_json(&serde_json::from_slice(&text).ok()?);
        Some(ImageIdentity {
            version: identity.version?,
            app: identity.app,
            mcu: identity.mcu?,
            can_frequency: identity.canbus_frequency_hz,
        })
    })
}

/// One application image the manifest lists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    /// The image's path, relative to the clone's `firmware` directory.
    pub path: String,
    pub probe_version: String,
    /// `USB` or `CAN`.
    pub protocol: String,
    pub can_speed: Option<u64>,
    pub lite: bool,
    pub version: semver::Version,
    /// The oldest `cartographer3d-plugin` the image works with, when it needs a recent one.
    pub min_plugin: Option<semver::Version>,
}

/// Why the manifest could not be read.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ManifestError {
    #[error("it has no header")]
    Empty,
    #[error("it has no {0:?} column")]
    MissingColumn(&'static str),
}

/// Parses `firmware_list.csv`, keeping only the images flashed onto an existing Katapult
/// (`process` `Update`, `firmware_type` `Cartographer`): never one that replaces or includes the
/// bootloader. Rows that cannot be read are skipped.
pub fn parse_manifest(text: &str) -> Result<Vec<ManifestEntry>, ManifestError> {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    let header: Vec<&str> = lines
        .next()
        .ok_or(ManifestError::Empty)?
        .split(',')
        .map(str::trim)
        .collect();
    let column = |name: &'static str| {
        header
            .iter()
            .position(|column| *column == name)
            .ok_or(ManifestError::MissingColumn(name))
    };
    let path = column("filepath")?;
    let firmware_type = column("firmware_type")?;
    let protocol = column("protocol")?;
    let process = column("process")?;
    let probe_version = column("probe_version")?;
    let firmware_version = column("firmware_version")?;
    let can_speed = column("can_speed")?;
    let is_lite = column("is_lite")?;
    let min_plugin = column("min_plugin_version")?;
    Ok(lines
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            if line.contains('"') {
                tracing::debug!(line, "skipping a quoted manifest row");
                return None;
            }
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let field = |index: usize| fields.get(index).copied().unwrap_or_default();
            if field(process) != "Update" || field(firmware_type) != "Cartographer" {
                return None;
            }
            let min_plugin = match field(min_plugin) {
                "" => None,
                version => Some(semver::Version::parse(version).ok()?),
            };
            Some(ManifestEntry {
                path: field(path).to_owned(),
                probe_version: field(probe_version).to_owned(),
                protocol: field(protocol).to_owned(),
                can_speed: match field(can_speed) {
                    "" => None,
                    speed => Some(speed.parse().ok()?),
                },
                lite: matches!(
                    field(is_lite).to_ascii_lowercase().as_str(),
                    "yes" | "true" | "1" | "y"
                ),
                version: semver::Version::parse(field(firmware_version)).ok()?,
                min_plugin,
            })
        })
        .collect())
}

/// The image one Cartographer is updated to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub version: semver::Version,
    /// The version the image reports once it runs, e.g. `CARTOGRAPHER V4 6.1.0`.
    pub reported: String,
    /// A newer image the installed plugin is too old for, with the plugin it needs.
    pub held_back: Option<(semver::Version, semver::Version)>,
}

/// Why no image is known for a Cartographer.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unavailable {
    #[error("no cartographer_firmware clone was found")]
    NoCheckout,
    #[error("could not read {path}: {reason}")]
    Manifest { path: PathBuf, reason: String },
    #[error("its connection is not configured")]
    NotConfigured,
    #[error("it is configured on CAN but did not report its CAN bitrate")]
    NoCanBitrate,
    #[error("it reports a CAN bitrate but is configured on {0}")]
    NotCan(String),
    #[error("the manifest lists no {0} image that matches it")]
    NoImage(String),
    #[error(
        "every matching image needs cartographer3d-plugin {needed} or newer, and {}",
        .installed.as_ref().map_or_else(|| "no installed version was found".to_owned(), |installed| format!("{installed} is installed"))
    )]
    PluginTooOld {
        needed: semver::Version,
        installed: Option<semver::Version>,
    },
    #[error("several images match it: {}", .0.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "))]
    Ambiguous(Vec<PathBuf>),
}

/// What each Cartographer in an inventory is updated to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Targets {
    /// The clone the images come from, when one was found.
    pub checkout: Option<PathBuf>,
    /// The installed `cartographer3d-plugin` version, when one was found.
    pub plugin: Option<semver::Version>,
    /// For each Cartographer MCU object, its image or why there is none.
    pub mcus: BTreeMap<String, Result<Target, Unavailable>>,
}

impl Targets {
    /// The image `name` is updated to, when one is known.
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.mcus.get(name)?.as_ref().ok()
    }
}

/// Why an image does not suit a probe; nothing that would be flashed may differ from it.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Mismatch {
    #[error("it holds no Cartographer identify dictionary")]
    NoIdentity,
    #[error("it is not Cartographer firmware")]
    NotCartographer,
    #[error("it is built for {image}, and the probe runs on {probe}")]
    Mcu { image: String, probe: String },
    #[error("it is built for {}, and the probe runs {}", link_label(*.image), link_label(*.probe))]
    Link {
        image: Option<u64>,
        probe: Option<u64>,
    },
    #[error("it is not linked to start at {APPLICATION_START:#x}, after Cartographer's Katapult")]
    NotAnApplication,
}

fn link_label(can_frequency: Option<u64>) -> String {
    can_frequency.map_or_else(|| "USB".to_owned(), |hz| format!("CAN at {hz} bit/s"))
}

/// Checks an image against the probe it would be flashed to: Cartographer firmware for the same MCU, the same link and
/// CAN bitrate, and an application linked above Cartographer's Katapult. The build (full or lite) is
/// the caller's to check, since an explicitly chosen file may change it. Returns what the image
/// says about itself.
pub fn check_image(image: &[u8], running: &Firmware) -> Result<ImageIdentity, Mismatch> {
    let identity = image_identity(image).ok_or(Mismatch::NoIdentity)?;
    if !names_itself_cartographer(identity.app.as_deref(), &identity.version) {
        return Err(Mismatch::NotCartographer);
    }
    if identity.mcu != running.mcu {
        return Err(Mismatch::Mcu {
            image: identity.mcu,
            probe: running.mcu.clone(),
        });
    }
    if identity.can_frequency != running.can_frequency {
        return Err(Mismatch::Link {
            image: identity.can_frequency,
            probe: running.can_frequency,
        });
    }
    if !crate::flash::stm32_dfu::plausible_application_starts(image).contains(&APPLICATION_START) {
        return Err(Mismatch::NotAnApplication);
    }
    Ok(identity)
}

/// Resolves a manifest path inside the clone's `firmware` directory, refusing one that would
/// lead outside it.
fn image_path(checkout: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    relative
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
        .then(|| checkout.join("firmware").join(relative))
}

/// Picks the image `running`, configured on `transport`, is updated to from the clone at
/// `checkout`: the newest version the installed plugin works with among the manifest's images
/// for its generation, link, bitrate and build, and only an image whose own identify dictionary
/// agrees. Exactly one image may match at that version.
pub fn select_target(
    checkout: &Path,
    manifest: &[ManifestEntry],
    running: &Firmware,
    transport: Option<&McuTransport>,
    plugin: Option<&semver::Version>,
) -> Result<Target, Unavailable> {
    let protocol = match (transport, running.can_frequency) {
        (None, _) => return Err(Unavailable::NotConfigured),
        (Some(McuTransport::Can { .. }), Some(_)) => "CAN",
        (Some(McuTransport::Can { .. }), None) => return Err(Unavailable::NoCanBitrate),
        (Some(McuTransport::Serial { device }), Some(_)) => {
            return Err(Unavailable::NotCan(format!("serial ({device})")));
        }
        (Some(McuTransport::Serial { .. }), None) => "USB",
    };
    let description = format!(
        "{} {} {} build",
        running.generation.label(),
        link_label(running.can_frequency),
        running.build.label()
    );
    let candidates: Vec<(&ManifestEntry, PathBuf, String)> = manifest
        .iter()
        .filter(|entry| {
            entry.probe_version == running.generation.label()
                && entry.protocol == protocol
                && entry.can_speed == running.can_frequency
                && entry.lite == (running.build == Build::Lite)
        })
        .filter_map(|entry| {
            let path = image_path(checkout, &entry.path)?;
            let image = std::fs::read(&path)
                .inspect_err(|error| {
                    tracing::debug!(%error, path = %path.display(), "could not read a listed image");
                })
                .ok()?;
            let identity = check_image(&image, running)
                .inspect_err(|reason| {
                    tracing::debug!(%reason, path = %path.display(), "skipping a listed image");
                })
                .ok()?;
            let agrees = Build::from_version(&identity.version) == running.build
                && parse_version(&identity.version).as_ref() == Some(&entry.version);
            if !agrees {
                tracing::debug!(
                    path = %path.display(),
                    reported = identity.version,
                    "skipping a listed image whose own version disagrees with the manifest"
                );
                return None;
            }
            Some((entry, path, identity.version))
        })
        .collect();
    let works_with_plugin = |entry: &ManifestEntry| {
        entry
            .min_plugin
            .as_ref()
            .is_none_or(|needed| plugin.is_some_and(|installed| installed >= needed))
    };
    let Some(best) = candidates
        .iter()
        .filter(|(entry, ..)| works_with_plugin(entry))
        .map(|(entry, ..)| &entry.version)
        .max()
    else {
        return Err(
            match candidates
                .iter()
                .filter_map(|(entry, ..)| entry.min_plugin.clone())
                .min()
            {
                Some(needed) => Unavailable::PluginTooOld {
                    needed,
                    installed: plugin.cloned(),
                },
                None => Unavailable::NoImage(description),
            },
        );
    };
    let mut at_best: Vec<_> = candidates
        .iter()
        .filter(|(entry, ..)| works_with_plugin(entry) && &entry.version == best)
        .collect();
    if at_best.len() != 1 {
        return Err(Unavailable::Ambiguous(
            at_best.iter().map(|(_, path, _)| path.clone()).collect(),
        ));
    }
    let (_, path, reported) = at_best.pop().expect("length was checked");
    let held_back = candidates
        .iter()
        .filter(|(entry, ..)| !works_with_plugin(entry) && &entry.version > best)
        .filter_map(|(entry, ..)| Some((entry.version.clone(), entry.min_plugin.clone()?)))
        .max();
    Ok(Target {
        path: path.clone(),
        version: best.clone(),
        reported: reported.clone(),
        held_back,
    })
}

/// The version of `cartographer3d-plugin` installed in the virtualenv `venv`, from its
/// `.dist-info` directory; a development version counts as its release (`1.5.3b2` is 1.5.3).
pub fn plugin_version_in(venv: &Path) -> Option<semver::Version> {
    let lib = std::fs::read_dir(venv.join("lib")).ok()?;
    lib.flatten()
        .filter_map(|python| std::fs::read_dir(python.path().join("site-packages")).ok())
        .flat_map(|packages| packages.flatten())
        .find_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("cartographer3d_plugin-")?
                .strip_suffix(".dist-info")?;
            let release: String = version
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let release = release.trim_end_matches('.');
            (release.split('.').count() == 3)
                .then(|| semver::Version::parse(release).ok())
                .flatten()
        })
}

/// Finds the user's `cartographer_firmware` clone: `configured` when given, otherwise
/// `~/cartographer_firmware`, where Cartographer's instructions clone it. Only a directory with a
/// `firmware_list.csv` counts.
pub fn locate_checkout(configured: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let candidate = match configured {
        Some(configured) => configured.to_owned(),
        None => home?.join("cartographer_firmware"),
    };
    candidate
        .join("firmware_list.csv")
        .is_file()
        .then_some(candidate)
}

/// Picks the image each Cartographer in `inventory` takes from the clone at `checkout`.
pub fn resolve_targets(
    inventory: &McuInventory,
    checkout: Option<&Path>,
    plugin: Option<semver::Version>,
) -> Targets {
    let cartographers: Vec<(&Mcu, Firmware)> = inventory
        .mcus
        .iter()
        .filter_map(|mcu| Some((mcu, identify(mcu)?.ok()?)))
        .collect();
    let manifest = checkout.map(|checkout| {
        let path = checkout.join("firmware_list.csv");
        std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse_manifest(&text).map_err(|error| error.to_string()))
            .map_err(|reason| Unavailable::Manifest { path, reason })
    });
    let mcus = cartographers
        .into_iter()
        .map(|(mcu, firmware)| {
            let target = match (checkout, &manifest) {
                (Some(checkout), Some(Ok(manifest))) => select_target(
                    checkout,
                    manifest,
                    &firmware,
                    mcu.transport.as_ref(),
                    plugin.as_ref(),
                ),
                (_, Some(Err(error))) => Err(error.clone()),
                _ => Err(Unavailable::NoCheckout),
            };
            (mcu.name.clone(), target)
        })
        .collect();
    Targets {
        checkout: checkout.map(Path::to_owned),
        plugin,
        mcus,
    }
}

/// What each Cartographer in `inventory` is updated to, when it has any: the images in the clone
/// [`locate_checkout`] finds, for the plugin installed beside Klippy's `python_path` (or in
/// `~/klippy-env`).
pub fn targets_for(
    inventory: &McuInventory,
    configured: Option<&Path>,
    python_path: Option<&Path>,
) -> Targets {
    if !inventory
        .mcus
        .iter()
        .any(|mcu| identify(mcu).is_some_and(|firmware| firmware.is_ok()))
    {
        return Targets::default();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let checkout = locate_checkout(configured, home.as_deref());
    let venvs = python_path
        .and_then(|python| python.parent()?.parent().map(Path::to_owned))
        .into_iter()
        .chain(home.map(|home| home.join("klippy-env")));
    let plugin = venvs.into_iter().find_map(|venv| plugin_version_in(&venv));
    tracing::debug!(?checkout, ?plugin, "cartographer_firmware clone located");
    resolve_targets(inventory, checkout.as_deref(), plugin)
}
