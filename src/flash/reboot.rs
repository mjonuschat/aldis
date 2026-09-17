//! Recovery: leaving an already-running bootloader without a full update.

use std::path::Path;
use std::time::Duration;

use crate::flash::usb_bootloader::{
    ObservedUsbBootloader, UsbBootloaderKind, scan_usb_bootloaders,
};
use crate::retry::retry_until_available;

/// One USB bootloader found on the bus, already classified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetectedBootloader {
    /// The bootloader's observed USB identity and topology.
    pub observed: ObservedUsbBootloader,
    /// The native backend that recognizes this identity.
    pub kind: UsbBootloaderKind,
}

/// Scans `root` for every USB device whose identity classifies as a known
/// bootloader, independent of Moonraker or any MCU's configured connection.
///
/// This is the entry point for recovering an MCU that is already stuck in a
/// bootloader: unlike the flashing path, it doesn't need to have observed the
/// MCU's application identity beforehand.
pub fn detect_bootloaders(root: &Path) -> Vec<DetectedBootloader> {
    scan_usb_bootloaders(root)
        .into_iter()
        .filter_map(|observed| {
            let kind = observed.kind()?;
            Some(DetectedBootloader { observed, kind })
        })
        .collect()
}

/// Polls `root` until at least one known bootloader is present or `timeout`
/// elapses, returning whatever was found (possibly empty).
pub fn wait_for_any_bootloader(
    root: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> Vec<DetectedBootloader> {
    retry_until_available(timeout, poll_interval, || {
        let detected = detect_bootloaders(root);
        if detected.is_empty() {
            Err(())
        } else {
            Ok(detected)
        }
    })
    .unwrap_or_default()
}

/// Polls `sysfs_path` until it no longer exists or `timeout` elapses.
///
/// Returns `true` once the path disappears (the device left, e.g. after a
/// successful reboot into its application), `false` if it was still present
/// when the timeout elapsed.
pub fn wait_until_absent(sysfs_path: &Path, timeout: Duration, poll_interval: Duration) -> bool {
    retry_until_available(timeout, poll_interval, || {
        if sysfs_path.exists() { Err(()) } else { Ok(()) }
    })
    .is_ok()
}
