//! Preparing the firmware of FOCI MCUs for a flash.
//!
//! These MCUs are not built from the Klipper checkout. For an update, the image is acquired from
//! where its vendor publishes it and checked again against what the user approved; for an
//! explicit `aldis flash <mcu> <file>`, the file the user names is checked against the MCU. Either
//! way the result is a [`FlashRequest`], the only thing the coordinator will flash for these
//! MCUs.

use crate::coordinator::{PendingBuild, UpdateProgress};
use crate::eligibility::{RevisionStatus, TargetVersions};
use crate::flash::request::{FlashRequest, Vendor};
use crate::foci::ReleasePort;
use crate::moonraker::{Mcu, McuInventory};

/// A running prebuilt-firmware build, identified from what the MCU reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Running {
    Foci(crate::foci::Firmware),
}

/// Why prebuilt firmware cannot be matched to an update image.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unsupported {
    /// FOCI firmware whose build no FOCI release asset matches.
    #[error(transparent)]
    Foci(crate::foci::Unsupported),
}

/// Identifies `mcu` as prebuilt firmware: `None` when it is not any vendor's, otherwise the
/// running build or why it cannot be matched. FOCI is recognized by its reported `app`.
pub fn identify(mcu: &Mcu) -> Option<Result<Running, Unsupported>> {
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
        }
    }
}

/// What the prebuilt-firmware MCUs of an inventory are updated to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Targets {
    /// The FOCI release FOCI MCUs are updated to; `None` when it is not known.
    pub foci: Option<crate::foci::Release>,
}

impl Running {
    /// The `supported:` line of `aldis status` for this MCU.
    pub fn supported_label(&self) -> String {
        match self {
            Self::Foci(firmware) => format!(
                "yes (FOCI {} {} release builds)",
                firmware.board.crate_name(),
                firmware.flavor.label()
            ),
        }
    }
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
        header
    }
}

/// The image one prebuilt-firmware MCU is updated to.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    Foci(&'a crate::foci::Release),
}

/// What `resolve_targets` needs to find the targets of an inventory's prebuilt-firmware MCUs.
pub struct ResolveContext {
    /// The latest FOCI release, already looked up (see [`crate::foci::release_for`]).
    pub foci: Option<crate::foci::Release>,
}

/// Finds what each prebuilt-firmware MCU of `inventory` is updated to: the FOCI release in
/// `context`.
pub fn resolve_targets(_inventory: &McuInventory, context: &ResolveContext) -> Targets {
    Targets {
        foci: context.foci.clone(),
    }
}

impl Targets {
    /// The target `running`, the MCU named `name`, is updated to, when one is known.
    pub fn target(&self, running: &Running) -> Option<Target<'_>> {
        match running {
            Running::Foci(_) => self.foci.as_ref().map(Target::Foci),
        }
    }

    /// How `running` compares with its target: `Indeterminate` when none is known.
    pub fn compare(&self, running: &Running) -> RevisionStatus {
        match (running, self.target(running)) {
            (Running::Foci(firmware), Some(Target::Foci(release))) => {
                crate::foci::compare(&firmware.version, release)
            }
            _ => RevisionStatus::Indeterminate,
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
        None
    }

    /// Whether `mcu` runs its target: the outer `None` when `mcu` is not prebuilt firmware, the
    /// inner `None` when no target is known to check against.
    pub fn reached_by(&self, mcu: &Mcu) -> Option<Option<bool>> {
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
        // Recognized but not matched to an update image: never fall through to a build.
        Err(reason) => Err(reason.to_string()),
    })
}

/// Prepares the file the user named for `aldis flash`: `bytes`, read before the user was asked,
/// checked against `mcu`. `None` when `mcu` is not prebuilt firmware, so any file may
/// go through the ordinary flash. A recognized MCU whose file does not suit it is an error; it
/// never falls through to the ordinary flash.
pub fn prepare_explicit(mcu: &Mcu, bytes: &[u8]) -> Result<Option<FlashRequest>, ExplicitError> {
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
