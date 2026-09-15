//! Shared USB sysfs topology helpers.

use std::path::{Path, PathBuf};

/// Walks the ancestors of `start` in sysfs, returning the nearest one that
/// exposes a complete USB device identity (`idVendor`, `idProduct`,
/// `busnum`, `devnum`).
pub(crate) fn usb_device_ancestor(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|candidate| {
        let has_usb_identity =
            candidate.join("idVendor").is_file() && candidate.join("idProduct").is_file();
        (has_usb_identity
            && candidate.join("busnum").is_file()
            && candidate.join("devnum").is_file())
        .then(|| candidate.to_path_buf())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::usb_device_ancestor;

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

    fn unique_temporary_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aldis-usb-sysfs-{}-{nonce}", std::process::id()))
    }
}
