//! Preparing the firmware of FOCI and Beacon MCUs for a flash.
//!
//! These MCUs are not built from the Klipper checkout. For an update, the image is acquired from
//! where its vendor publishes it and checked again against what the user approved; for an
//! explicit `aldis flash <mcu> <file>`, the file the user names is checked against the MCU. Either
//! way the result is a [`FlashRequest`], the only thing the coordinator will flash for these
//! MCUs.

use std::path::Path;

use crate::coordinator::{PendingBuild, UpdateProgress};
use crate::eligibility::{RevisionStatus, TargetVersions};
use crate::flash::beacon_dfu::ObservedBootloader;
use crate::flash::request::{FlashRequest, RecoveryRequest, Vendor};
use crate::foci::ReleasePort;
use crate::moonraker::{Mcu, McuInventory};

/// A running prebuilt-firmware build, identified from what the MCU reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Running {
    Foci(crate::foci::Firmware),
    Beacon(crate::beacon::Firmware),
}

/// Why prebuilt firmware cannot be matched to an update image.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unsupported {
    /// FOCI firmware whose build no FOCI release asset matches.
    #[error(transparent)]
    Foci(crate::foci::Unsupported),
}

/// Identifies `mcu` as prebuilt firmware: `None` when it is not any vendor's, otherwise the
/// running build or why it cannot be matched. A Beacon is recognized first, then FOCI by its
/// reported `app`.
pub fn identify(mcu: &Mcu) -> Option<Result<Running, Unsupported>> {
    if let Some(firmware) = crate::beacon::identify(mcu) {
        return Some(Ok(Running::Beacon(firmware)));
    }
    if crate::foci::is_foci(mcu) {
        return Some(
            crate::foci::identify(mcu)
                .map(Running::Foci)
                .map_err(Unsupported::Foci),
        );
    }
    None
}

impl Running {
    pub fn vendor(&self) -> Vendor {
        match self {
            Self::Foci(_) => Vendor::Foci,
            Self::Beacon(_) => Vendor::Beacon,
        }
    }
}

/// What the prebuilt-firmware MCUs of an inventory are updated to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Targets {
    /// The FOCI release FOCI MCUs are updated to; `None` when it is not known.
    pub foci: Option<crate::foci::Release>,
    /// The image each Beacon is updated to, or why none is known.
    pub beacon: crate::beacon::Targets,
}

impl Running {
    /// The `supported:` line of `aldis status` for this MCU.
    pub fn supported_label(&self, targets: &Targets, name: &str) -> String {
        match self {
            Self::Beacon(_) => match targets.beacon.mcus.get(name) {
                Some(Ok(target)) => format!(
                    "yes ({} holds {})",
                    file_label(&target.path),
                    crate::beacon::reported(&target.version)
                ),
                Some(Err(reason)) => format!("yes (no Beacon image known: {reason})"),
                None => "yes (no Beacon image known)".to_owned(),
            },
            Self::Foci(firmware) => format!(
                "yes (FOCI {} {} release builds)",
                firmware.board.crate_name(),
                firmware.flavor.label()
            ),
        }
    }
}

/// The name of an image file, for display.
fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

impl Targets {
    /// The lines `aldis status` prints above the MCUs for each vendor present in `inventory`:
    /// what its firmware comes from.
    pub fn status_header(&self, inventory: &McuInventory) -> String {
        let mut header = String::new();
        if inventory.mcus.iter().any(crate::foci::is_foci) {
            header.push_str(&format!(
                "FOCI release:      {}\n",
                self.foci
                    .as_ref()
                    .map_or("unknown", |release| release.tag.as_str())
            ));
        }
        if inventory.mcus.iter().any(crate::beacon::is_beacon) {
            header.push_str(&format!(
                "Beacon firmware:   {}\n",
                self.beacon.checkout.as_ref().map_or_else(
                    || "no beacon_klipper checkout found".to_owned(),
                    |checkout| checkout.display().to_string()
                )
            ));
        }
        header
    }
}

/// The image one prebuilt-firmware MCU is updated to.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    Foci(&'a crate::foci::Release),
    Beacon(&'a crate::beacon::Target),
}

/// What `resolve_targets` needs to find the targets of an inventory's prebuilt-firmware MCUs.
pub struct ResolveContext<'a> {
    /// The latest FOCI release, already looked up (see [`crate::foci::release_for`]).
    pub foci: Option<crate::foci::Release>,
    /// Where Beacon firmware is read from, when configured.
    pub beacon_source: Option<&'a Path>,
    /// The Klipper checkout, which Beacon's installer links its module from.
    pub klipper_source: Option<&'a Path>,
}

/// Finds what each prebuilt-firmware MCU of `inventory` is updated to: the FOCI release in
/// `context` and the Beacon image for each revision from its checkout. Looks only for the vendors
/// present.
pub fn resolve_targets(inventory: &McuInventory, context: &ResolveContext<'_>) -> Targets {
    Targets {
        foci: context.foci.clone(),
        beacon: crate::beacon::targets_for(
            inventory,
            context.beacon_source,
            context.klipper_source,
        ),
    }
}

impl Targets {
    /// The target `running`, the MCU named `name`, is updated to, when one is known.
    pub fn target(&self, running: &Running, name: &str) -> Option<Target<'_>> {
        match running {
            Running::Foci(_) => self.foci.as_ref().map(Target::Foci),
            Running::Beacon(_) => self.beacon.target(name).map(Target::Beacon),
        }
    }

    /// How `running` compares with its target: `Indeterminate` when none is known.
    pub fn compare(&self, running: &Running, name: &str) -> RevisionStatus {
        match (running, self.target(running, name)) {
            (Running::Foci(firmware), Some(Target::Foci(release))) => {
                crate::foci::compare(&firmware.version, release)
            }
            (Running::Beacon(firmware), Some(Target::Beacon(target))) => {
                crate::beacon::compare(&firmware.version, &target.version)
            }
            _ => RevisionStatus::Indeterminate,
        }
    }

    /// Whether an update may replace `running`, the MCU named `name`, with its target. A Beacon
    /// is never downgraded by an update, whatever the selection; `flash` with a named file is the
    /// only way. True when no target is known (there is nothing to refuse), and for the vendors
    /// that have no such rule.
    pub fn admits(&self, running: &Running, name: &str) -> bool {
        match (running, self.target(running, name)) {
            (Running::Beacon(firmware), Some(Target::Beacon(target))) => {
                crate::beacon::admits(&firmware.version, &target.version)
            }
            _ => true,
        }
    }

    /// Why an update will not replace `running`, the MCU named `name`, with its target, when it
    /// is because that would be a downgrade.
    pub fn downgrade_refusal(&self, running: &Running, name: &str) -> Option<String> {
        match (running, self.target(running, name)) {
            (Running::Beacon(firmware), Some(Target::Beacon(target)))
                if !crate::beacon::admits(&firmware.version, &target.version) =>
            {
                Some(format!(
                    "{name} runs Beacon {}, newer than the Beacon {} in its beacon_klipper \
                     checkout, and an update never downgrades it: name the file with `aldis \
                     flash {} <file>` to install the older image",
                    firmware.version,
                    target.version,
                    crate::agent::api::display_name(name)
                ))
            }
            _ => None,
        }
    }

    /// The target version `mcu` is updated to, for display; `None` when `mcu` is not prebuilt
    /// firmware, `unknown` when it is but no target is known. Judged by what the MCU names
    /// itself, so firmware aldis cannot compare still counts.
    pub fn label_for(&self, mcu: &Mcu) -> Option<String> {
        let unknown = || "unknown".to_owned();
        if crate::foci::is_foci(mcu) {
            return Some(
                self.foci
                    .as_ref()
                    .map_or_else(unknown, |release| release.tag.clone()),
            );
        }
        if crate::beacon::is_beacon(mcu) {
            return Some(
                self.beacon
                    .target(&mcu.name)
                    .map_or_else(unknown, |target| crate::beacon::reported(&target.version)),
            );
        }
        None
    }

    /// Whether `mcu` runs its target: the outer `None` when `mcu` is not prebuilt firmware, the
    /// inner `None` when no target is known to check against.
    pub fn reached_by(&self, mcu: &Mcu) -> Option<Option<bool>> {
        if crate::beacon::is_beacon(mcu) {
            return Some(self.beacon.target(&mcu.name).map(|target| {
                crate::beacon::identify(mcu)
                    .is_some_and(|firmware| firmware.version == target.version)
            }));
        }
        if crate::foci::is_foci(mcu) {
            return Some(self.foci.as_ref().map(|release| {
                crate::foci::identify(mcu).is_ok_and(|firmware| {
                    crate::foci::compare(&firmware.version, release) == RevisionStatus::Current
                })
            }));
        }
        None
    }
}

/// Why a file named for a prebuilt-firmware MCU cannot be flashed to it.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExplicitError {
    /// A Beacon takes only a Beacon DFU image.
    #[error(transparent)]
    NotBeaconImage(#[from] crate::beacon::ImageError),
    /// FOCI publishes nothing for this board, so the address its firmware starts at is unknown.
    #[error(
        "FOCI publishes no builds for {0} boards, so aldis cannot tell where its firmware must start"
    )]
    UnknownFociBoard(String),
}

/// Prepares the update `mcu` was approved for: downloads or reads its image, checks it once more
/// against the running MCU and the version the user approved, and keeps a best-effort copy beside
/// the run's other artifacts. `None` when `mcu` is not prebuilt firmware (it is built from the
/// Klipper checkout); otherwise a request, or the reason there is no image to flash. Nothing has
/// been written to the MCU when this fails.
pub fn prepare_automatic(
    mcu: &Mcu,
    versions: &TargetVersions,
    releases: &dyn ReleasePort,
    pending: &PendingBuild,
    progress: &mut dyn FnMut(UpdateProgress),
) -> Option<Result<FlashRequest, String>> {
    let name = mcu.name.as_str();
    Some(match identify(mcu)? {
        Ok(Running::Foci(firmware)) => download_foci_firmware(
            &firmware, versions, releases, pending, progress, name,
        )
        .map(|image| {
            FlashRequest::katapult(Vendor::Foci, mcu, image, firmware.board.application_start())
        }),
        Ok(Running::Beacon(firmware)) => {
            read_beacon_image(&firmware, versions, pending, progress, name)
                .map(|image| FlashRequest::beacon(mcu, image))
        }
        // Recognized but not matched to an update image: never fall through to a build.
        Err(reason) => Err(reason.to_string()),
    })
}

/// Prepares the file the user named for `aldis flash`: `bytes`, read before the user was asked,
/// checked against `mcu`. `None` when `mcu` is not prebuilt firmware, so any file may
/// go through the ordinary flash. A recognized MCU whose file does not suit it is an error; it
/// never falls through to the ordinary flash.
pub fn prepare_explicit(mcu: &Mcu, bytes: &[u8]) -> Result<Option<FlashRequest>, ExplicitError> {
    // A Beacon whose version aldis cannot compare (a beta) still takes only a Beacon image.
    if crate::beacon::names_itself_beacon(mcu) {
        let image = crate::beacon::parse_image(bytes)?;
        return Ok(Some(FlashRequest::beacon(mcu, image)));
    }
    if crate::foci::is_foci(mcu) {
        let board = crate::foci::Board::from_mcu(&mcu.mcu)
            .ok_or_else(|| ExplicitError::UnknownFociBoard(mcu.mcu.clone()))?;
        return Ok(Some(FlashRequest::katapult(
            Vendor::Foci,
            mcu,
            bytes.to_vec(),
            board.application_start(),
        )));
    }
    Ok(None)
}

/// Prepares the image that brings a Beacon out of `bootloader`: the one in `checkout` for the
/// revision the bootloader names, which must be for this very bootloader.
pub fn prepare_beacon_recovery(
    checkout: Option<&Path>,
    bootloader: &ObservedBootloader,
) -> Result<RecoveryRequest, crate::beacon::Unavailable> {
    let (path, image) =
        crate::beacon::recovery_image(checkout, &bootloader.product, &bootloader.usb_id())?;
    RecoveryRequest::new(bootloader.clone(), path, image)
}

/// Downloads the FOCI release image that replaces `firmware` and keeps a copy beside the run's
/// other artifacts. Returns the failure's description when there is no image to flash.
fn download_foci_firmware(
    firmware: &crate::foci::Firmware,
    versions: &TargetVersions,
    releases: &dyn ReleasePort,
    pending: &PendingBuild,
    progress: &mut dyn FnMut(UpdateProgress),
    name: &str,
) -> Result<Vec<u8>, String> {
    let release = versions
        .prebuilt
        .foci
        .as_ref()
        .ok_or_else(|| format!("no FOCI release is known to update {name} to"))?;
    let asset = release.asset_name(firmware.board, firmware.flavor);
    progress(UpdateProgress::DownloadingFirmware);
    let image = releases
        .download_firmware(release, &asset)
        .map_err(|error| {
            tracing::debug!(?error, "FOCI firmware download failed");
            format!(
                "could not download {asset} from FOCI {}: {}",
                release.tag,
                crate::error_chain(&error)
            )
        })?;
    progress(UpdateProgress::FirmwareDownloaded);
    let copy = pending.artifact_path().with_file_name(&asset);
    if let Err(error) = std::fs::write(&copy, &image) {
        tracing::debug!(?error, path = %copy.display(), "could not keep a copy of the firmware");
    }
    Ok(image)
}

/// Reads the Beacon image `name` was approved for from the checkout and keeps a copy beside the
/// run's other artifacts. Returns the failure's description when there is no image to flash,
/// including when the checkout changed to another version since the image was assessed.
fn read_beacon_image(
    running: &crate::beacon::Firmware,
    versions: &TargetVersions,
    pending: &PendingBuild,
    progress: &mut dyn FnMut(UpdateProgress),
    name: &str,
) -> Result<crate::beacon::FirmwareImage, String> {
    let target = versions
        .prebuilt
        .beacon
        .target(name)
        .ok_or_else(|| format!("no Beacon firmware image is known to update {name} with"))?;
    progress(UpdateProgress::ReadingFirmware);
    let image = crate::beacon::read_image(&target.path).map_err(|error| error.to_string())?;
    if image.version != target.version {
        return Err(format!(
            "{} now holds Beacon {}, not the Beacon {} {name} was to be updated to",
            target.path.display(),
            image.version,
            target.version
        ));
    }
    if !crate::beacon::admits(&running.version, &image.version) {
        return Err(format!(
            "refusing to downgrade {name} from Beacon {} to Beacon {}; name the file with \
             `aldis flash` to install an older image",
            running.version, image.version
        ));
    }
    progress(UpdateProgress::FirmwareRead);
    if let Some(file) = target.path.file_name() {
        let copy = pending.artifact_path().with_file_name(file);
        if let Err(error) = std::fs::copy(&target.path, &copy) {
            tracing::debug!(?error, path = %copy.display(), "could not keep a copy of the firmware");
        }
    }
    Ok(image)
}
