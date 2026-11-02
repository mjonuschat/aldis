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

    use super::{usb_device_ancestor, usb_topology_snapshot};

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
}
