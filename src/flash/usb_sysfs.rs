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

/// A device-level snapshot of USB descriptor fields read from sysfs, scoped
/// to exactly one topology path. Used to diagnose an unsupported or
/// unexpected bootloader identity; never enumerates sibling USB devices.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UsbTopologySnapshot {
    pub(crate) device_class: String,
    pub(crate) device_subclass: String,
    pub(crate) device_protocol: String,
    pub(crate) usb_version: String,
    pub(crate) device_version: String,
    pub(crate) max_packet_size0: String,
    pub(crate) num_configurations: String,
    pub(crate) product: String,
    pub(crate) serial: String,
    pub(crate) interfaces: Vec<UsbInterfaceSnapshot>,
}

/// One USB interface's descriptor fields and endpoints.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UsbInterfaceSnapshot {
    pub(crate) number: String,
    pub(crate) class: String,
    pub(crate) subclass: String,
    pub(crate) protocol: String,
    pub(crate) endpoints: Vec<UsbEndpointSnapshot>,
}

/// One USB endpoint's descriptor fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UsbEndpointSnapshot {
    pub(crate) address: String,
    pub(crate) attributes: String,
    pub(crate) max_packet_size: String,
}

/// Reads a diagnostic snapshot of the USB device at `usb_path`: its own
/// descriptor fields and every interface and endpoint at that exact
/// topology. A missing or unreadable sysfs attribute yields an empty
/// string rather than failing the capture; this is best-effort diagnostic
/// data, never a new source of failure.
pub(crate) fn usb_topology_snapshot(usb_path: &Path) -> UsbTopologySnapshot {
    UsbTopologySnapshot {
        device_class: read_sysfs_attribute(usb_path, "bDeviceClass"),
        device_subclass: read_sysfs_attribute(usb_path, "bDeviceSubClass"),
        device_protocol: read_sysfs_attribute(usb_path, "bDeviceProtocol"),
        usb_version: read_sysfs_attribute(usb_path, "bcdUSB"),
        device_version: read_sysfs_attribute(usb_path, "bcdDevice"),
        max_packet_size0: read_sysfs_attribute(usb_path, "bMaxPacketSize0"),
        num_configurations: read_sysfs_attribute(usb_path, "bNumConfigurations"),
        product: read_sysfs_attribute(usb_path, "product"),
        serial: read_sysfs_attribute(usb_path, "serial"),
        interfaces: usb_interfaces(usb_path),
    }
}

fn usb_interfaces(usb_path: &Path) -> Vec<UsbInterfaceSnapshot> {
    let Some(name) = usb_path.file_name() else {
        return Vec::new();
    };
    let prefix = format!("{}:", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(usb_path) else {
        return Vec::new();
    };
    let mut interfaces = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| UsbInterfaceSnapshot {
            number: read_sysfs_attribute(&entry.path(), "bInterfaceNumber"),
            class: read_sysfs_attribute(&entry.path(), "bInterfaceClass"),
            subclass: read_sysfs_attribute(&entry.path(), "bInterfaceSubClass"),
            protocol: read_sysfs_attribute(&entry.path(), "bInterfaceProtocol"),
            endpoints: usb_endpoints(&entry.path()),
        })
        .collect::<Vec<_>>();
    interfaces.sort_by(|a, b| a.number.cmp(&b.number));
    interfaces
}

fn usb_endpoints(interface_path: &Path) -> Vec<UsbEndpointSnapshot> {
    let Ok(entries) = fs::read_dir(interface_path) else {
        return Vec::new();
    };
    let mut endpoints = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("ep_"))
        .map(|entry| UsbEndpointSnapshot {
            address: read_sysfs_attribute(&entry.path(), "bEndpointAddress"),
            attributes: read_sysfs_attribute(&entry.path(), "bmAttributes"),
            max_packet_size: read_sysfs_attribute(&entry.path(), "wMaxPacketSize"),
        })
        .collect::<Vec<_>>();
    endpoints.sort_by(|a, b| a.address.cmp(&b.address));
    endpoints
}

fn read_sysfs_attribute(path: &Path, attribute: &str) -> String {
    fs::read_to_string(path.join(attribute))
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
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
    fn captures_device_and_interface_descriptors_at_one_topology() {
        let usb_path = unique_temporary_path();
        let interface = usb_path.join(format!(
            "{}:1.0",
            usb_path.file_name().unwrap().to_string_lossy()
        ));
        let endpoint = interface.join("ep_81");
        fs::create_dir_all(&endpoint).expect("create sysfs fixture");
        fs::write(usb_path.join("bDeviceClass"), "ef\n").expect("write bDeviceClass");
        fs::write(usb_path.join("bcdUSB"), "0200\n").expect("write bcdUSB");
        fs::write(usb_path.join("product"), "Example Device\n").expect("write product");
        fs::write(interface.join("bInterfaceNumber"), "00\n").expect("write bInterfaceNumber");
        fs::write(interface.join("bInterfaceClass"), "0a\n").expect("write bInterfaceClass");
        fs::write(endpoint.join("bEndpointAddress"), "81\n").expect("write bEndpointAddress");
        fs::write(endpoint.join("wMaxPacketSize"), "0040\n").expect("write wMaxPacketSize");

        let snapshot = usb_topology_snapshot(&usb_path);

        assert_eq!(snapshot.device_class, "ef");
        assert_eq!(snapshot.usb_version, "0200");
        assert_eq!(snapshot.product, "Example Device");
        assert_eq!(snapshot.interfaces.len(), 1);
        assert_eq!(snapshot.interfaces[0].number, "00");
        assert_eq!(snapshot.interfaces[0].class, "0a");
        assert_eq!(snapshot.interfaces[0].endpoints.len(), 1);
        assert_eq!(snapshot.interfaces[0].endpoints[0].address, "81");
        assert_eq!(snapshot.interfaces[0].endpoints[0].max_packet_size, "0040");

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    #[test]
    fn leaves_missing_attributes_empty_instead_of_failing() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");

        let snapshot = usb_topology_snapshot(&usb_path);

        assert_eq!(snapshot.device_class, "");
        assert_eq!(snapshot.serial, "");
        assert!(snapshot.interfaces.is_empty());

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
