//! Beacon's DFU bootloader: entered with Klipper's 1200-baud touch, then sent a plain DFU
//! download, as Beacon's `update_firmware.py` does with `dfu-util`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use dfu_nusb::DfuNusb;

use crate::beacon::VENDOR_ID;
use crate::flash::FlashResult;
use crate::flash::katapult::serial::{SystemSerialIo, usb_device_path};
use crate::retry::retry_until_available;

/// The product ids Beacon's bootloader enumerates with, one per hardware revision family.
pub const BOOTLOADER_PRODUCT_IDS: [u16; 2] = [0xbeac, 0xbea1];

/// A Beacon bootloader at one USB topology.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedBootloader {
    /// Linux sysfs path identifying the physical USB topology.
    pub sysfs_path: PathBuf,
    pub vendor_id: u16,
    pub product_id: u16,
    /// The bootloader's USB product string, which names the Beacon's revision.
    pub product: String,
}

impl ObservedBootloader {
    /// The bootloader's USB id, as `vvvv:pppp`.
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

/// Failure while entering or flashing Beacon's bootloader.
#[derive(Debug, thiserror::Error)]
pub enum BeaconDfuError {
    /// The configured serial device could not be related to a USB device.
    #[error("{} is not a USB device", .0.display())]
    NotUsbDevice(PathBuf),
    /// No Beacon bootloader appeared at the device's USB topology before the timeout.
    #[error(
        "the Beacon at {} did not enter its bootloader{}",
        .device.display(),
        .reset_error.as_deref().map(|error| format!(" (the serial request reported: {error})")).unwrap_or_default()
    )]
    NotEntered {
        device: PathBuf,
        reset_error: Option<String>,
    },
    /// USB enumeration or opening the bootloader failed.
    #[error("could not open the Beacon bootloader: {0}")]
    Discovery(#[source] nusb::Error),
    /// The bootloader disappeared, or exposed no DFU interface.
    #[error("no Beacon DFU bootloader {0} was found at its USB topology")]
    DeviceNotFound(String),
    /// The DFU transfer failed.
    #[error("the Beacon bootloader rejected or failed the firmware download: {0}")]
    Transport(#[source] dfu_nusb::Error),
}

/// Whether a USB device with this identity is Beacon's bootloader. As `update_firmware.py`, the
/// product string must name the bootloader, since the identity alone may not differ from the
/// application's.
pub fn is_bootloader(vendor_id: u16, product_id: u16, product: &str) -> bool {
    vendor_id == VENDOR_ID
        && BOOTLOADER_PRODUCT_IDS.contains(&product_id)
        && product.contains("Bootloader")
}

fn read_hex_id(path: &Path) -> Option<u16> {
    u16::from_str_radix(std::fs::read_to_string(path).ok()?.trim(), 16).ok()
}

/// The Beacon bootloader at `usb_path`, if that is what is there now.
pub fn bootloader_at(usb_path: &Path) -> Option<ObservedBootloader> {
    let vendor_id = read_hex_id(&usb_path.join("idVendor"))?;
    let product_id = read_hex_id(&usb_path.join("idProduct"))?;
    let product = std::fs::read_to_string(usb_path.join("product")).ok()?;
    let product = product.trim();
    is_bootloader(vendor_id, product_id, product).then(|| ObservedBootloader {
        sysfs_path: usb_path.to_owned(),
        vendor_id,
        product_id,
        product: product.to_owned(),
    })
}

/// Every Beacon bootloader among the USB devices directly under `root` (normally
/// `/sys/bus/usb/devices`), such as one an interrupted flash left behind.
pub fn scan_bootloaders(root: &Path) -> Vec<ObservedBootloader> {
    crate::flash::usb_sysfs::usb_device_dirs(root)
        .iter()
        .filter_map(|path| bootloader_at(path))
        .collect()
}

/// Whether the device at `usb_path` is a Beacon running its application, with its serial port.
pub fn application_at(usb_path: &Path) -> bool {
    let vendor = read_hex_id(&usb_path.join("idVendor"));
    let product = std::fs::read_to_string(usb_path.join("product")).unwrap_or_default();
    let product = product.trim();
    vendor == Some(VENDOR_ID)
        && product.starts_with("Beacon Rev")
        && !product.contains("Bootloader")
        && crate::flash::usb_sysfs::usb_tty(usb_path).is_some()
}

/// Waits until the Beacon at `usb_path` runs its application again, serial port included.
/// Returns whether it did before `timeout`.
pub fn wait_for_application(usb_path: &Path, timeout: Duration, poll_interval: Duration) -> bool {
    retry_until_available(timeout, poll_interval, || {
        application_at(usb_path).then_some(()).ok_or(())
    })
    .is_ok()
}

/// Waits until the device at `usb_path` is no longer a Beacon bootloader: it reset into its
/// application, or left the bus. Returns whether it did before `timeout`.
pub fn wait_until_left(usb_path: &Path, timeout: Duration, poll_interval: Duration) -> bool {
    retry_until_available(timeout, poll_interval, || {
        bootloader_at(usb_path).map_or(Ok(()), |_| Err(()))
    })
    .is_ok()
}

/// Asks the Beacon on serial port `device` to enter its bootloader with a 1200-baud touch and
/// waits for the bootloader to appear at the same USB topology.
pub fn request_and_observe_bootloader(
    device: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<ObservedBootloader, BeaconDfuError> {
    let usb_path =
        usb_device_path(device).ok_or_else(|| BeaconDfuError::NotUsbDevice(device.to_owned()))?;
    let reset_error = SystemSerialIo::request_usb_bootloader(device)
        .err()
        .map(|error| error.to_string());
    retry_until_available(timeout, poll_interval, || {
        bootloader_at(&usb_path).ok_or(()).inspect_err(|()| {
            tracing::debug!(path = %usb_path.display(), "beacon bootloader not yet observed, still waiting");
        })
    })
    .map_err(|()| BeaconDfuError::NotEntered {
        device: device.to_owned(),
        reset_error,
    })
}

/// Downloads `payload` to `bootloader` and starts its manifestation. The bootloader resets into
/// the new application afterwards, and a transfer error while it does so is expected: Beacon's
/// own updater ignores those too. Whether the new firmware runs is for the caller to verify.
pub fn flash(
    bootloader: &ObservedBootloader,
    payload: &[u8],
) -> Result<FlashResult, BeaconDfuError> {
    crate::flash::block_on(async {
        let not_found = || BeaconDfuError::DeviceNotFound(bootloader.usb_id());
        let mut matches = nusb::list_devices()
            .await
            .map_err(BeaconDfuError::Discovery)?
            .filter(|device| {
                device.sysfs_path() == bootloader.sysfs_path
                    && device.vendor_id() == bootloader.vendor_id
                    && device.product_id() == bootloader.product_id
            });
        let device_info = matches.next().ok_or_else(not_found)?;
        let interface_number = device_info
            .interfaces()
            .find(|interface| interface.class() == 0xfe && interface.subclass() == 0x01)
            .map(|interface| interface.interface_number())
            .ok_or_else(not_found)?;
        let device = device_info
            .open()
            .await
            .map_err(BeaconDfuError::Discovery)?;
        let interface = device
            .detach_and_claim_interface(interface_number)
            .await
            .map_err(BeaconDfuError::Discovery)?;
        let dfu = DfuNusb::open(device, interface, 0)
            .await
            .map_err(BeaconDfuError::Transport)?
            .into_sync_dfu()
            .download_without_manifest_from_slice(payload)
            .map_err(BeaconDfuError::Transport)?;
        match dfu.manifest() {
            Ok(_) => {}
            Err(error @ (dfu_nusb::Error::Transfer(_) | dfu_nusb::Error::Nusb(_))) => {
                tracing::debug!(%error, "beacon bootloader dropped off the bus during manifestation");
            }
            Err(error) => return Err(BeaconDfuError::Transport(error)),
        }
        Ok(FlashResult {
            reported_pages: None,
            padded_bytes: payload.len(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{bootloader_at, is_bootloader};

    #[test]
    fn recognizes_the_bootloader_only_by_its_identity_and_product_name() {
        assert!(is_bootloader(0x04d8, 0xbeac, "Beacon RevD Bootloader"));
        assert!(is_bootloader(0x04d8, 0xbea1, "Beacon RevH Bootloader"));
        assert!(!is_bootloader(0x04d8, 0xbea1, "Beacon RevH"));
        assert!(!is_bootloader(0x04d8, 0x1234, "Beacon RevH Bootloader"));
        assert!(!is_bootloader(0x0483, 0xbea1, "Beacon RevH Bootloader"));
    }

    #[test]
    fn reads_the_bootloader_identity_from_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        let usb = dir.path();
        std::fs::write(usb.join("idVendor"), "04d8\n").unwrap();
        std::fs::write(usb.join("idProduct"), "bea1\n").unwrap();
        std::fs::write(usb.join("product"), "Beacon RevH\n").unwrap();
        assert_eq!(
            bootloader_at(usb),
            None,
            "the application is not the bootloader"
        );

        std::fs::write(usb.join("product"), "Beacon RevH Bootloader\n").unwrap();
        let observed = bootloader_at(usb).unwrap();
        assert_eq!(observed.usb_id(), "04d8:bea1");
        assert_eq!(observed.sysfs_path, usb);
        assert_eq!(observed.product, "Beacon RevH Bootloader");
    }

    #[test]
    fn finds_beacon_bootloaders_on_the_bus_and_sees_them_leave() {
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let device = |name: &str, id: &str, product: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("idVendor"), "04d8\n").unwrap();
            std::fs::write(path.join("idProduct"), format!("{id}\n")).unwrap();
            std::fs::write(path.join("product"), format!("{product}\n")).unwrap();
            path
        };
        let stuck = device("1-1", "beac", "Beacon RevD Bootloader");
        device("1-2", "bea1", "Beacon RevH");

        let found = super::scan_bootloaders(root.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].usb_id(), "04d8:beac");
        assert_eq!(found[0].sysfs_path, std::fs::canonicalize(&stuck).unwrap());

        let short = Duration::from_millis(20);
        assert!(!super::wait_until_left(
            &stuck,
            short,
            Duration::from_millis(5)
        ));
        std::fs::write(stuck.join("product"), "Beacon RevD\n").unwrap();
        assert!(super::wait_until_left(
            &stuck,
            short,
            Duration::from_millis(5)
        ));
        assert!(
            !super::application_at(&stuck),
            "the application is back only once its serial port is"
        );
        let tty = stuck.join("1-1:1.0").join("tty").join("ttyACM0");
        std::fs::create_dir_all(&tty).unwrap();
        assert!(super::wait_for_application(
            &stuck,
            short,
            Duration::from_millis(5)
        ));
    }
}
