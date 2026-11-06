//! Beacon firmware: which firmware file a running Beacon takes from the user's `beacon_klipper`
//! checkout.
//!
//! [Beacon](https://github.com/beacon3d/beacon_klipper) is an eddy-current probe that speaks
//! Klipper's MCU protocol. Its firmware is closed source and is not built from the Klipper
//! checkout: it ships as one encrypted DFU image per hardware revision (`firmware/rev<x>.dfu`) in
//! the `beacon_klipper` checkout, which is updated with `git pull`. aldis reads that image from
//! the user's own checkout and never downloads or redistributes it.
//!
//! As Beacon's own `update_firmware.py` does, the image is chosen by the revision the device names
//! in its USB product string (`Beacon RevH ...`). A running version older than the image's is an
//! update. One newer than the image's (an older commit is checked out) is never replaced by an
//! update: that downgrade is made only by naming the file with `aldis flash`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::eligibility::RevisionStatus;
use crate::moonraker::{Mcu, McuInventory, McuTransport};

/// The USB vendor Beacon's application and bootloader use (Microchip's).
pub const VENDOR_ID: u16 = 0x04d8;

/// The word Beacon's firmware starts its reported version with.
const NAME: &str = "Beacon";

/// Parses the version Beacon reports to Klipper, e.g. `Beacon 2.1.0`: the word `Beacon` and three
/// plain numbers that each fit a byte, as Beacon's firmware writes them.
pub fn parse_reported(version: &str) -> Option<Version> {
    let mut words = version.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case(NAME) {
        return None;
    }
    let number = words.next()?;
    if words.next().is_some() {
        return None;
    }
    let mut parts = number.split('.');
    let mut part = || -> Option<u8> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let parsed = Version::new(u64::from(part()?), u64::from(part()?), u64::from(part()?));
    parts.next().is_none().then_some(parsed)
}

/// Decodes the version a Beacon image's DFU suffix (and the device's own `bcdDevice`) holds, as
/// Beacon's `update_firmware.py` does: `0x2010` is 2.1.0 and `0x2100` is 2.10.0.
pub fn version_from_bcd(bcd: u16) -> Version {
    let [low, high] = bcd.to_le_bytes();
    Version::new(
        u64::from(high >> 4),
        u64::from((high & 0x0f) * 10 + (low >> 4)),
        u64::from(low & 0x0f),
    )
}

/// The version as Beacon reports it to Klipper, e.g. `Beacon 2.1.0`.
pub fn reported(version: &Version) -> String {
    format!("{NAME} {version}")
}

/// A running Beacon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Firmware {
    pub version: Version,
}

/// Whether `mcu` runs Beacon firmware aldis can compare: it names itself only in its version,
/// `Beacon <major>.<minor>.<patch>`, and reports an empty `app` (or, should a later release fill
/// it in, `Beacon`).
pub fn is_beacon(mcu: &Mcu) -> bool {
    identify(mcu).is_some()
}

/// The running Beacon `mcu` reports, or `None` when it is not one aldis can compare.
pub fn identify(mcu: &Mcu) -> Option<Firmware> {
    if !mcu
        .app_name()
        .is_none_or(|app| app.eq_ignore_ascii_case(NAME))
    {
        return None;
    }
    let version = parse_reported(mcu.version.as_deref()?)?;
    Some(Firmware { version })
}

/// Whether `mcu` names itself Beacon, including with a version aldis cannot compare (such as a
/// beta). Its firmware is still flashed only through Beacon's bootloader.
pub fn names_itself_beacon(mcu: &Mcu) -> bool {
    mcu.app_name()
        .is_none_or(|app| app.eq_ignore_ascii_case(NAME))
        && mcu
            .version_name()
            .is_some_and(|word| word.eq_ignore_ascii_case(NAME))
}

/// How a running Beacon compares with the image in the checkout. A Beacon newer than the image
/// is not downgraded by an update, so it cannot be placed relative to it.
pub fn compare(running: &Version, target: &Version) -> RevisionStatus {
    match running.cmp(target) {
        std::cmp::Ordering::Equal => RevisionStatus::Current,
        std::cmp::Ordering::Less => RevisionStatus::UpdateRequired,
        std::cmp::Ordering::Greater => RevisionStatus::Indeterminate,
    }
}

/// Whether an update may replace the running Beacon with the image holding `target`: never with
/// an older one. Reflashing the same version is allowed.
pub fn admits(running: &Version, target: &Version) -> bool {
    target >= running
}

/// The settings section that configures the Beacon whose MCU object is `object`: `beacon.py`
/// names the MCU `mcu <section>` for its `[beacon]` and `[beacon sensor <name>]` sections.
pub fn config_section(object: &str) -> Option<&str> {
    let section = object.strip_prefix("mcu ")?;
    (section == "beacon" || section.starts_with("beacon sensor ")).then_some(section)
}

/// The hardware revision a Beacon's USB product string names, lowercased as its firmware file
/// names it: `Beacon RevH` is `h`. Matches `update_firmware.py`, and accepts only letters and
/// digits so a revision can never name a path outside the checkout's `firmware` directory.
pub fn revision_from_product(product: &str) -> Option<String> {
    let rest = product.strip_prefix("Beacon ")?.to_ascii_lowercase();
    let revision = rest.strip_prefix("rev")?.split(' ').next()?;
    (!revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then(|| revision.to_owned())
}

/// The length of a DFU file suffix.
const SUFFIX_LENGTH: usize = 16;
/// The signature every DFU file suffix carries, as stored.
const SUFFIX_SIGNATURE: &[u8; 3] = b"UFD";

/// A Beacon firmware image: the payload sent to the bootloader and what its DFU suffix says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirmwareImage {
    /// The bytes downloaded to the bootloader: the file without its DFU suffix.
    pub payload: Vec<u8>,
    /// The bootloader the image is for.
    pub vendor_id: u16,
    pub product_id: u16,
    /// The firmware version the image holds.
    pub version: Version,
}

impl FirmwareImage {
    /// The bootloader the image is for, as `vvvv:pppp`.
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

/// Why a file is not a usable Beacon firmware image.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ImageError {
    #[error("it is too short to be a DFU file")]
    TooShort,
    #[error("it has no DFU suffix")]
    NoSuffix,
    #[error("its DFU suffix checksum does not match its contents")]
    ChecksumMismatch,
    #[error("its DFU suffix names USB vendor {0:04x}, not Beacon's {VENDOR_ID:04x}")]
    WrongVendor(u16),
    #[error("it holds no firmware")]
    Empty,
}

/// Parses a Beacon `.dfu` file: a raw payload followed by a standard DFU suffix, whose CRC must
/// match and whose vendor must be Beacon's.
pub fn parse_image(file: &[u8]) -> Result<FirmwareImage, ImageError> {
    if file.len() < SUFFIX_LENGTH {
        return Err(ImageError::TooShort);
    }
    let suffix = &file[file.len() - SUFFIX_LENGTH..];
    let word = |offset: usize| u16::from_le_bytes([suffix[offset], suffix[offset + 1]]);
    if &suffix[8..11] != SUFFIX_SIGNATURE {
        return Err(ImageError::NoSuffix);
    }
    let length = usize::from(suffix[11]);
    if length < SUFFIX_LENGTH || length > file.len() {
        return Err(ImageError::NoSuffix);
    }
    let crc = u32::from_le_bytes(suffix[12..16].try_into().expect("four bytes"));
    if dfu_crc(&file[..file.len() - 4]) != crc {
        return Err(ImageError::ChecksumMismatch);
    }
    let vendor_id = word(4);
    if vendor_id != VENDOR_ID {
        return Err(ImageError::WrongVendor(vendor_id));
    }
    let payload = file[..file.len() - length].to_vec();
    if payload.is_empty() {
        return Err(ImageError::Empty);
    }
    Ok(FirmwareImage {
        payload,
        vendor_id,
        product_id: word(2),
        version: version_from_bcd(word(0)),
    })
}

/// The CRC a DFU suffix stores: CRC-32 without its final inversion.
fn dfu_crc(bytes: &[u8]) -> u32 {
    bytes.iter().fold(u32::MAX, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| {
            if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            }
        })
    })
}

/// Finds the user's `beacon_klipper` checkout: `configured` when given, otherwise the directory
/// Beacon's installer linked `klippy/extras/beacon.py` in the Klipper checkout from, otherwise
/// `~/beacon_klipper`, where Beacon's instructions clone it. Only a directory with a `firmware`
/// directory counts.
pub fn locate_checkout(
    configured: Option<&Path>,
    klipper_source: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let has_firmware = |path: &Path| path.join("firmware").is_dir();
    if let Some(configured) = configured {
        return has_firmware(configured).then(|| configured.to_owned());
    }
    let linked = klipper_source
        .and_then(|source| std::fs::canonicalize(source.join("klippy/extras/beacon.py")).ok())
        .and_then(|module| module.parent().map(Path::to_owned));
    linked
        .into_iter()
        .chain(home.map(|home| home.join("beacon_klipper")))
        .find(|path| has_firmware(path))
}

/// The firmware file for one hardware revision in a checkout.
pub fn image_path(checkout: &Path, revision: &str) -> PathBuf {
    checkout.join("firmware").join(format!("rev{revision}.dfu"))
}

/// The image one Beacon is updated to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// The hardware revision the device names, lowercased.
    pub revision: String,
    /// The firmware file for that revision.
    pub path: PathBuf,
    /// The version the file holds.
    pub version: Version,
}

/// Why no image is known for a Beacon.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unavailable {
    #[error("no beacon_klipper checkout was found")]
    NoCheckout,
    #[error("its serial port is not configured")]
    NotConfigured,
    #[error("Beacon firmware is flashed over USB only, and it is configured on {0}")]
    NotUsb(String),
    #[error("could not read the USB product name of {0}")]
    NoProduct(String),
    #[error("its USB product name {0:?} names no Beacon revision")]
    UnknownRevision(String),
    #[error("could not read {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("{path} is not a Beacon firmware image: {reason}")]
    InvalidImage { path: PathBuf, reason: ImageError },
    #[error("{path} is for bootloader {image}, not {bootloader}")]
    WrongBootloader {
        path: PathBuf,
        image: String,
        bootloader: String,
    },
}

/// What each Beacon in an inventory is updated to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Targets {
    /// The checkout the images come from, when one was found.
    pub checkout: Option<PathBuf>,
    /// For each Beacon MCU object, its image or why there is none.
    pub mcus: BTreeMap<String, Result<Target, Unavailable>>,
}

impl Targets {
    /// The image `name` is updated to, when one is known.
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.mcus.get(name)?.as_ref().ok()
    }
}

/// Reads and parses the image at `path`.
pub fn read_image(path: &Path) -> Result<FirmwareImage, Unavailable> {
    let file = std::fs::read(path).map_err(|error| Unavailable::Unreadable {
        path: path.to_owned(),
        reason: error.to_string(),
    })?;
    parse_image(&file).map_err(|reason| Unavailable::InvalidImage {
        path: path.to_owned(),
        reason,
    })
}

/// Finds the image each Beacon in `inventory` takes from `checkout`. `usb_product` reads the USB
/// product string of the device behind a serial port.
pub fn resolve_targets(
    inventory: &McuInventory,
    checkout: Option<&Path>,
    usb_product: impl Fn(&Path) -> Option<String>,
) -> Targets {
    let mcus = inventory
        .mcus
        .iter()
        .filter(|mcu| is_beacon(mcu))
        .map(|mcu| {
            let target = checkout
                .ok_or(Unavailable::NoCheckout)
                .and_then(|checkout| resolve_target(mcu, checkout, &usb_product));
            (mcu.name.clone(), target)
        })
        .collect();
    Targets {
        checkout: checkout.map(Path::to_owned),
        mcus,
    }
}

fn resolve_target(
    mcu: &Mcu,
    checkout: &Path,
    usb_product: &impl Fn(&Path) -> Option<String>,
) -> Result<Target, Unavailable> {
    let device = match &mcu.transport {
        Some(McuTransport::Serial { device }) => device,
        Some(transport @ McuTransport::Can { .. }) => {
            return Err(Unavailable::NotUsb(transport.describe()));
        }
        None => return Err(Unavailable::NotConfigured),
    };
    let product =
        usb_product(Path::new(device)).ok_or_else(|| Unavailable::NoProduct(device.clone()))?;
    let revision =
        revision_from_product(&product).ok_or(Unavailable::UnknownRevision(product.clone()))?;
    let path = image_path(checkout, &revision);
    let image = read_image(&path)?;
    Ok(Target {
        revision,
        path,
        version: image.version,
    })
}

/// The image that brings a Beacon out of its bootloader `usb_id`, whose USB product string is
/// `product` (`Beacon RevH Bootloader`): the one in `checkout` for the revision it names, which
/// must be for this bootloader.
pub fn recovery_image(
    checkout: Option<&Path>,
    product: &str,
    usb_id: &str,
) -> Result<(PathBuf, FirmwareImage), Unavailable> {
    let checkout = checkout.ok_or(Unavailable::NoCheckout)?;
    let revision =
        revision_from_product(product).ok_or(Unavailable::UnknownRevision(product.to_owned()))?;
    let path = image_path(checkout, &revision);
    let image = read_image(&path)?;
    if image.usb_id() != usb_id {
        return Err(Unavailable::WrongBootloader {
            path,
            image: image.usb_id(),
            bootloader: usb_id.to_owned(),
        });
    }
    Ok((path, image))
}

/// The checkout [`locate_checkout`] finds from the environment: `configured`, the one
/// `klipper_source` links `beacon.py` from, or `~/beacon_klipper`.
pub fn find_checkout(configured: Option<&Path>, klipper_source: Option<&Path>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    locate_checkout(configured, klipper_source, home.as_deref())
}

/// The USB product string of the device behind the serial port `device`.
pub fn usb_product(device: &Path) -> Option<String> {
    let usb = crate::flash::katapult::serial::usb_device_path(device)?;
    let product = std::fs::read_to_string(usb.join("product")).ok()?;
    Some(product.trim().to_owned())
}

/// What each Beacon in `inventory` is updated to, when it has any: the images in the checkout
/// found by [`locate_checkout`], matched to each device through sysfs.
pub fn targets_for(
    inventory: &McuInventory,
    configured: Option<&Path>,
    klipper_source: Option<&Path>,
) -> Targets {
    if !inventory.mcus.iter().any(is_beacon) {
        return Targets::default();
    }
    let checkout = find_checkout(configured, klipper_source);
    tracing::debug!(?checkout, "beacon_klipper checkout located");
    resolve_targets(inventory, checkout.as_deref(), usb_product)
}

#[cfg(test)]
mod tests {
    use super::dfu_crc;

    #[test]
    fn computes_the_crc_dfu_suffixes_store() {
        // CRC-32 of "123456789" is 0xcbf43926; a DFU suffix stores it uninverted.
        assert_eq!(dfu_crc(b"123456789"), !0xcbf4_3926);
    }
}
