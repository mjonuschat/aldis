//! Shared USB sysfs topology helpers.

use std::fs;
use std::path::{Path, PathBuf};

/// Walks the ancestors of `start` in sysfs, returning the nearest one that
/// exposes a complete USB device identity (`idVendor`, `idProduct`,
/// `busnum`, `devnum`).
pub(crate) fn usb_device_ancestor(start: &Path) -> Option<PathBuf> {
    let ancestor = start.ancestors().find_map(|candidate| {
        let has_usb_identity =
            candidate.join("idVendor").is_file() && candidate.join("idProduct").is_file();
        (has_usb_identity
            && candidate.join("busnum").is_file()
            && candidate.join("devnum").is_file())
        .then(|| candidate.to_path_buf())
    });

    if let Some(sysfs_path) = &ancestor {
        tracing::debug!(
            start = %start.display(),
            sysfs_path = %sysfs_path.display(),
            "usb device sysfs ancestor resolved"
        );
    }

    ancestor
}

/// The USB device descriptor fields the unsupported-bootloader report shows, read from sysfs
/// at exactly one topology path; never enumerates sibling USB devices.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UsbTopologySnapshot {
    pub(crate) device_class: String,
    pub(crate) device_subclass: String,
    pub(crate) device_protocol: String,
    pub(crate) product: String,
    pub(crate) serial: String,
}

/// Reads the device descriptor fields of the USB device at `usb_path`. A missing or unreadable
/// sysfs attribute yields an empty string rather than failing the capture; this is best-effort
/// diagnostic data, never a new source of failure.
pub(crate) fn usb_topology_snapshot(usb_path: &Path) -> UsbTopologySnapshot {
    UsbTopologySnapshot {
        device_class: read_sysfs_attribute(usb_path, "bDeviceClass"),
        device_subclass: read_sysfs_attribute(usb_path, "bDeviceSubClass"),
        device_protocol: read_sysfs_attribute(usb_path, "bDeviceProtocol"),
        product: read_sysfs_attribute(usb_path, "product"),
        serial: read_sysfs_attribute(usb_path, "serial"),
    }
}

fn read_sysfs_attribute(path: &Path, attribute: &str) -> String {
    fs::read_to_string(path.join(attribute))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// Lowercase USB vendor:product id and manufacturer read directly from sysfs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UsbIdentity {
    pub(crate) usb_id: String,
    pub(crate) manufacturer: String,
}

impl UsbIdentity {
    /// Returns whether both the vendor and product identifiers are present.
    ///
    /// A device mid-re-enumeration briefly reports an empty identity; this
    /// distinguishes that transient state from a real (if unsupported) one.
    pub(crate) fn is_complete(&self) -> bool {
        self.usb_id
            .split_once(':')
            .is_some_and(|(vendor, product)| !vendor.is_empty() && !product.is_empty())
    }
}

/// Consecutive identical reads required before a changed identity counts as settled.
pub(crate) const SETTLED_IDENTITY_READS: usize = 5;

/// Debounces a USB identity polled from sysfs while a device re-enumerates.
///
/// sysfs exposes a device's attributes piecemeal as it tears down and comes back: the
/// vendor/product ids can be empty, or present while `manufacturer` has already vanished. A
/// single read is therefore not evidence of a new device. A poll loop feeds every read to
/// [`observe`](Self::observe), which reports an identity only once it differs from the
/// pre-reset one, is complete, and has been read identically several times in a row.
///
/// A read with the pre-reset vendor:product id and an empty manufacturer is never a new
/// device: sysfs can drop `manufacturer` for an arbitrarily long stretch while the device
/// re-enumerates. A same-vendor:product read with a different, non-empty manufacturer is
/// positive evidence of a new device, which covers a bootloader built with the same custom
/// USB id as its application and told apart only by its manufacturer string.
#[derive(Debug)]
pub(crate) struct UsbIdentitySettler {
    initial: UsbIdentity,
    required_reads: usize,
    candidate: Option<UsbIdentity>,
    streak: usize,
}

impl UsbIdentitySettler {
    /// Watches for the device to settle on an identity other than `initial`.
    pub(crate) fn new(initial: UsbIdentity) -> Self {
        Self::with_required_reads(initial, SETTLED_IDENTITY_READS)
    }

    pub(crate) fn with_required_reads(initial: UsbIdentity, required_reads: usize) -> Self {
        Self {
            initial,
            required_reads: required_reads.max(1),
            candidate: None,
            streak: 0,
        }
    }

    /// A read that keeps the initial vendor:product id but has lost its manufacturer.
    fn is_manufacturer_dropout(&self, current: &UsbIdentity) -> bool {
        current.usb_id == self.initial.usb_id && current.manufacturer.is_empty()
    }

    /// Records one read; returns the identity once it has settled.
    pub(crate) fn observe(&mut self, current: &UsbIdentity) -> Option<&UsbIdentity> {
        if *current == self.initial
            || !current.is_complete()
            || self.is_manufacturer_dropout(current)
        {
            self.candidate = None;
            self.streak = 0;
            return None;
        }
        if self.candidate.as_ref() == Some(current) {
            self.streak += 1;
        } else {
            self.candidate = Some(current.clone());
            self.streak = 1;
        }
        if self.streak >= self.required_reads {
            self.candidate.as_ref()
        } else {
            None
        }
    }
}

/// Reads one USB device's identity directly from its sysfs attributes.
pub(crate) fn usb_identity(usb_path: &Path) -> UsbIdentity {
    UsbIdentity {
        usb_id: format!(
            "{}:{}",
            read_sysfs_value(&usb_path.join("idVendor")),
            read_sysfs_value(&usb_path.join("idProduct"))
        ),
        manufacturer: read_sysfs_value(&usb_path.join("manufacturer")),
    }
}

fn read_sysfs_value(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The interface directories (`<device>:<configuration>.<number>`) of the USB device at `usb_path`
/// that sysfs has exposed so far. They appear after the device's own attributes, once the device
/// is configured.
fn interface_dirs(usb_path: &Path) -> impl Iterator<Item = PathBuf> {
    let prefix = format!(
        "{}:",
        usb_path.file_name().unwrap_or_default().to_string_lossy()
    );
    fs::read_dir(usb_path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(move |entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
}

/// Whether sysfs has exposed the class of any interface of the USB device at `usb_path` yet. The
/// directory of an interface exists a moment before its attributes can be read.
pub(crate) fn has_interfaces(usb_path: &Path) -> bool {
    interface_dirs(usb_path).any(|interface| {
        !read_sysfs_attribute(&interface, "bInterfaceClass").is_empty()
            && !read_sysfs_attribute(&interface, "bInterfaceSubClass").is_empty()
    })
}

/// Whether the USB device at `usb_path` has a DFU interface (class `0xFE`, subclass `1`), which is
/// what a DFU bootloader exposes and a running application does not. Unlike the device's ids and
/// strings, which firmware builds choose, this separates a bootloader from the application that
/// shares its ids.
pub(crate) fn has_dfu_interface(usb_path: &Path) -> bool {
    interface_dirs(usb_path).any(|interface| {
        read_sysfs_attribute(&interface, "bInterfaceClass") == "fe"
            && read_sysfs_attribute(&interface, "bInterfaceSubClass") == "01"
    })
}

/// Finds the bootloader's unique tty device directly under its interface subtree.
pub(crate) fn usb_tty(usb_path: &Path) -> Option<PathBuf> {
    let prefix = format!("{}:", usb_path.file_name()?.to_string_lossy());
    let mut tty_names = fs::read_dir(usb_path)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .flat_map(|interface| {
            fs::read_dir(interface.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter(|entry| entry.file_name() == "tty")
        .flat_map(|tty_dir| fs::read_dir(tty_dir.path()).into_iter().flatten().flatten())
        .map(|tty| tty.file_name())
        .filter(|name| name.to_string_lossy().starts_with("tty"));
    let tty = tty_names.next()?;
    tty_names
        .next()
        .is_none()
        .then(|| Path::new("/dev").join(tty))
}

/// Lists top-level USB device directories directly under `root`, canonicalized.
///
/// Filters out USB interface entries (e.g. `1-1.4:1.0`), which share the
/// same directory but never carry a complete device identity of their own.
/// `/sys/bus/usb/devices/*` entries are symlinks; callers elsewhere in this
/// codebase (and `nusb::DeviceInfo::sysfs_path`) work in canonical
/// `/sys/devices/...` paths, so results are canonicalized here too — a
/// symlink path would never compare equal to either.
pub(crate) fn usb_device_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("idVendor").is_file() && path.join("idProduct").is_file())
        .filter_map(|path| fs::canonicalize(&path).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        UsbIdentity, UsbIdentitySettler, has_dfu_interface, has_interfaces, usb_device_ancestor,
        usb_topology_snapshot,
    };

    fn identity(usb_id: &str, manufacturer: &str) -> UsbIdentity {
        UsbIdentity {
            usb_id: usb_id.to_owned(),
            manufacturer: manufacturer.to_owned(),
        }
    }

    fn settler() -> UsbIdentitySettler {
        UsbIdentitySettler::with_required_reads(identity("1d50:606f", "klipper"), 3)
    }

    #[test]
    fn settles_only_after_the_same_changed_identity_repeats() {
        let mut settler = settler();
        let katapult = identity("1d50:6177", "katapult");

        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), Some(&katapult));
    }

    #[test]
    fn a_one_read_manufacturer_loss_does_not_settle() {
        let mut settler = settler();
        let katapult = identity("1d50:6177", "katapult");

        assert_eq!(settler.observe(&identity("1d50:606f", "")), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), Some(&katapult));
    }

    #[test]
    fn an_interruption_restarts_the_count() {
        let mut settler = settler();
        let katapult = identity("1d50:6177", "katapult");

        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&identity(":", "")), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), Some(&katapult));
    }

    #[test]
    fn a_manufacturer_loss_never_settles_however_long_it_lasts() {
        let mut settler = settler();
        let stripped = identity("1d50:606f", "");

        for _ in 0..50 {
            assert_eq!(settler.observe(&stripped), None);
        }
        let katapult = identity("1d50:6177", "katapult");
        for _ in 0..2 {
            assert_eq!(settler.observe(&katapult), None);
        }
        assert_eq!(settler.observe(&katapult), Some(&katapult));
    }

    #[test]
    fn a_new_manufacturer_on_the_same_usb_id_settles() {
        let mut settler = settler();
        let katapult = identity("1d50:606f", "katapult");

        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), None);
        assert_eq!(settler.observe(&katapult), Some(&katapult));
    }

    #[test]
    fn the_initial_identity_never_settles() {
        let mut settler = settler();
        let initial = identity("1d50:606f", "klipper");

        for _ in 0..10 {
            assert_eq!(settler.observe(&initial), None);
        }
    }

    #[test]
    fn finds_the_nearest_ancestor_with_a_complete_usb_identity() {
        let root = unique_temporary_path();
        let leaf = root.join("1-1").join("1-1:1.0").join("tty").join("ttyACM0");
        fs::create_dir_all(&leaf).expect("create sysfs fixture");
        let device_dir = root.join("1-1");
        fs::write(device_dir.join("idVendor"), "1234\n").expect("write idVendor");
        fs::write(device_dir.join("idProduct"), "5678\n").expect("write idProduct");
        fs::write(device_dir.join("busnum"), "1\n").expect("write busnum");
        fs::write(device_dir.join("devnum"), "5\n").expect("write devnum");

        assert_eq!(usb_device_ancestor(&leaf), Some(device_dir));

        fs::remove_dir_all(root).expect("remove sysfs fixture");
    }

    #[test]
    fn returns_none_when_no_ancestor_has_a_complete_usb_identity() {
        let root = unique_temporary_path();
        let leaf = root.join("1-1").join("tty").join("ttyACM0");
        fs::create_dir_all(&leaf).expect("create sysfs fixture");

        assert_eq!(usb_device_ancestor(&leaf), None);

        fs::remove_dir_all(root).expect("remove sysfs fixture");
    }

    #[test]
    fn captures_the_device_descriptors_at_one_topology() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        fs::write(usb_path.join("bDeviceClass"), "ef\n").expect("write bDeviceClass");
        fs::write(usb_path.join("product"), "Example Device\n").expect("write product");
        fs::write(usb_path.join("serial"), "ABC123\n").expect("write serial");

        let snapshot = usb_topology_snapshot(&usb_path);

        assert_eq!(snapshot.device_class, "ef");
        assert_eq!(snapshot.product, "Example Device");
        assert_eq!(snapshot.serial, "ABC123");

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    #[test]
    fn leaves_missing_attributes_empty_instead_of_failing() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");

        let snapshot = usb_topology_snapshot(&usb_path);

        assert_eq!(snapshot.device_class, "");
        assert_eq!(snapshot.serial, "");

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    fn unique_temporary_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aldis-usb-sysfs-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn tells_a_dfu_interface_from_a_cdc_one_whatever_the_device_strings_say() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("1-1");
        let interface = |name: &str, class: &str, subclass: &str| {
            let path = device.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("bInterfaceClass"), format!("{class}\n")).unwrap();
            std::fs::write(path.join("bInterfaceSubClass"), format!("{subclass}\n")).unwrap();
        };
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("manufacturer"), "Bondtech AB\n").unwrap();
        assert!(!has_dfu_interface(&device), "no interfaces yet");
        assert!(!has_interfaces(&device));

        std::fs::create_dir_all(device.join("1-1:1.0")).unwrap();
        assert!(
            !has_interfaces(&device),
            "an interface directory is not ready before its attributes are"
        );
        std::fs::remove_dir_all(device.join("1-1:1.0")).unwrap();

        interface("1-1:1.0", "02", "02");
        interface("1-1:1.1", "0a", "00");
        assert!(!has_dfu_interface(&device), "a CDC device is no bootloader");

        interface("1-1:1.2", "fe", "01");
        assert!(has_dfu_interface(&device));
        assert!(has_interfaces(&device));
        assert!(
            !has_dfu_interface(&dir.path().join("1-2")),
            "a missing device"
        );
    }
}
