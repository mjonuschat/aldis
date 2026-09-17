//! Katapult serial transport framing.
//!
//! The bootloader entry sequence and protocol framing follow
//! <https://github.com/Arksine/katapult/blob/master/scripts/flashtool.py>.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use std::fs;
use std::path::PathBuf;

use super::MAX_RESPONSE_FRAME_BYTES;
use super::session::Transport;
use crate::flash::usb_bootloader::{ObservedUsbBootloader, UsbBootloaderKind};
use crate::flash::usb_sysfs::usb_device_ancestor;
use crate::retry::retry_until_available;

/// Klipper's explicit request to reboot a serial MCU into its bootloader.
pub const BOOTLOADER_ENTRY_REQUEST: &[u8] = b"~ \x1c Request Serial Bootloader!! ~";

/// Returns whether USB identity fields identify a Katapult bootloader.
pub fn is_katapult_usb(usb_id: &str, manufacturer: &str) -> bool {
    matches!(
        crate::flash::usb_bootloader::classify_usb_identity(usb_id, manufacturer),
        Some(UsbBootloaderKind::Katapult)
    )
}

/// Sends and receives bounded byte chunks from a serial device.
pub trait SerialIo {
    /// The serial implementation's error type.
    type Error;

    /// Sends all bytes from one Katapult request frame.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Receives the next non-empty serial chunk.
    fn read(&mut self) -> Result<Vec<u8>, Self::Error>;
}

/// An error while exchanging Katapult protocol frames over serial.
#[derive(Debug)]
pub enum SerialTransportError<E> {
    /// The underlying serial implementation failed.
    Io(E),
    /// The response is larger than Katapult can represent.
    ResponseTooLarge {
        /// Number of bytes observed before rejecting the response.
        length: usize,
    },
    /// The response exceeded its payload-length declaration.
    ResponseLengthExceeded {
        /// Length declared in the response header.
        expected: usize,
        /// Number of bytes actually received.
        actual: usize,
    },
}

impl fmt::Display for SerialTransportError<io::Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "serial transport error"),
            Self::ResponseTooLarge { length } => write!(
                f,
                "serial response of {length} bytes exceeds the maximum {MAX_RESPONSE_FRAME_BYTES} bytes"
            ),
            Self::ResponseLengthExceeded { expected, actual } => write!(
                f,
                "serial response of {actual} bytes exceeds the {expected} bytes declared by its header"
            ),
        }
    }
}

impl std::error::Error for SerialTransportError<io::Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::ResponseTooLarge { .. } | Self::ResponseLengthExceeded { .. } => None,
        }
    }
}

/// A sequential Katapult transport over serial bytes.
pub struct KatapultSerialTransport<T> {
    io: T,
}

impl<T> KatapultSerialTransport<T> {
    /// Creates a transport over an already-open serial I/O implementation.
    pub fn new(io: T) -> Self {
        Self { io }
    }

    /// Returns the underlying serial I/O implementation.
    pub fn into_io(self) -> T {
        self.io
    }
}

impl<T: SerialIo> Transport for KatapultSerialTransport<T> {
    type Error = SerialTransportError<T::Error>;

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        self.io
            .write_all(request)
            .map_err(SerialTransportError::Io)?;
        let mut response = Vec::new();
        let mut expected = None;
        loop {
            let chunk = self.io.read().map_err(SerialTransportError::Io)?;
            let length = response.len() + chunk.len();
            if length > MAX_RESPONSE_FRAME_BYTES {
                return Err(SerialTransportError::ResponseTooLarge { length });
            }
            response.extend_from_slice(&chunk);
            if expected.is_none() && response.len() >= 4 {
                expected = Some(8 + usize::from(response[3]) * 4);
            }
            if let Some(expected) = expected {
                if response.len() == expected {
                    return Ok(response);
                }
                if response.len() > expected {
                    return Err(SerialTransportError::ResponseLengthExceeded {
                        expected,
                        actual: response.len(),
                    });
                }
            }
        }
    }
}

/// A system serial device opened at an explicitly supplied path and baud rate.
pub struct SystemSerialIo {
    port: Box<dyn serialport::SerialPort>,
}

impl SystemSerialIo {
    /// Opens a serial device with a bounded read timeout.
    pub fn open(path: &Path, baud_rate: u32, read_timeout: Duration) -> serialport::Result<Self> {
        let port = serialport::new(path.to_string_lossy(), baud_rate)
            .timeout(read_timeout)
            .open()?;
        Ok(Self { port })
    }

    /// Requests USB CDC bootloader entry using Klipper's 1200-baud DTR pulse.
    pub fn request_usb_bootloader(path: &Path) -> serialport::Result<()> {
        let mut port = serialport::new(path.to_string_lossy(), 1200).open()?;
        port.write_data_terminal_ready(true)?;
        port.set_baud_rate(1200)?;
        // A successful reset may disconnect the USB device before DTR clears.
        let _ = port.write_data_terminal_ready(false);
        Ok(())
    }

    /// Requests USB bootloader entry and waits for Katapult on the same USB path.
    ///
    /// On Linux this follows Katapult's topology-based strategy: the current
    /// tty is resolved into sysfs, its USB device is watched through
    /// re-enumeration, and the new tty is selected only from that device. A
    /// reset I/O error does not stop the wait because disconnecting the old
    /// USB CDC device is an expected successful outcome.
    pub fn request_and_find_usb_bootloader(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<PathBuf, UsbBootloaderError> {
        Self::request_and_find_usb_identity(path, timeout, poll_interval, is_katapult_usb)
    }

    /// Requests USB bootloader entry and waits for a matching identity at the same topology.
    pub fn request_and_find_usb_identity(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Result<PathBuf, UsbBootloaderError> {
        Self::request_usb_bootloader_and_wait(path, timeout, poll_interval, matches, usb_tty)
    }

    /// Requests USB bootloader entry and returns the observed bootloader at the same topology.
    pub fn request_and_observe_usb_bootloader(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Result<ObservedUsbBootloader, UsbBootloaderError> {
        Self::request_usb_bootloader_and_wait(path, timeout, poll_interval, matches, |usb_path| {
            let identity = usb_identity(usb_path);
            usb_tty(usb_path).map(|serial_device| ObservedUsbBootloader {
                sysfs_path: usb_path.to_path_buf(),
                usb_id: identity.usb_id,
                manufacturer: identity.manufacturer,
                serial_device: Some(serial_device),
            })
        })
    }

    /// Requests USB bootloader entry and observes any re-enumerated identity.
    ///
    /// Callers must classify the observation before selecting a flashing
    /// backend. This avoids treating the application Kconfig as evidence of
    /// which bootloader is installed.
    pub fn request_and_observe_any_usb_bootloader(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<ObservedUsbBootloader, UsbBootloaderError> {
        Self::request_and_observe_usb_bootloader(path, timeout, poll_interval, |_, _| true)
    }

    /// Observes the USB bootloader that replaces an already identified USB device.
    ///
    /// This is used when a running MCU is itself a USB CAN bridge. Entering its
    /// bootloader removes the SocketCAN interface, but the USB topology remains
    /// stable and can be used to select the bootloader's serial endpoint.
    pub fn observe_any_usb_bootloader_at_path(
        usb_path: &Path,
        initial_usb_id: &str,
        initial_manufacturer: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<ObservedUsbBootloader, UsbBootloaderError> {
        let initial_identity = UsbIdentity {
            usb_id: initial_usb_id.to_owned(),
            manufacturer: initial_manufacturer.to_owned(),
        };
        let mut last_identity = initial_identity.clone();
        retry_until_available(timeout, poll_interval, || {
            let identity = usb_identity(usb_path);
            last_identity = identity.clone();
            if identity != initial_identity
                && identity.is_complete()
                && let Some(serial_device) = usb_tty(usb_path)
            {
                Ok(ObservedUsbBootloader {
                    sysfs_path: usb_path.to_path_buf(),
                    usb_id: identity.usb_id,
                    manufacturer: identity.manufacturer,
                    serial_device: Some(serial_device),
                })
            } else {
                tracing::debug!(
                    usb_id = %identity.usb_id,
                    manufacturer = %identity.manufacturer,
                    "usb bootloader not yet observed, still waiting"
                );
                Err(())
            }
        })
        .map_err(|()| timeout_outcome(&initial_identity, &last_identity, usb_path, None))
    }

    /// Requests USB bootloader entry and waits for a matching USB identity.
    pub fn request_and_wait_for_usb_identity(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Result<(), UsbBootloaderError> {
        Self::request_usb_bootloader_and_wait(path, timeout, poll_interval, matches, |_| Some(()))
    }

    fn request_usb_bootloader_and_wait<T>(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
        ready: impl Fn(&Path) -> Option<T>,
    ) -> Result<T, UsbBootloaderError> {
        let usb_path = usb_device_path(path).ok_or_else(|| UsbBootloaderError::NotUsbDevice {
            device: path.to_path_buf(),
        })?;
        let initial_identity = usb_identity(&usb_path);
        let reset_error = Self::request_usb_bootloader(path)
            .err()
            .map(|error| error.to_string());

        let mut last_identity = initial_identity.clone();
        retry_until_available(timeout, poll_interval, || {
            let identity = usb_identity(&usb_path);
            last_identity = identity.clone();
            if identity != initial_identity
                && identity.is_complete()
                && matches(&identity.usb_id, &identity.manufacturer)
                && let Some(result) = ready(&usb_path)
            {
                Ok(result)
            } else {
                tracing::debug!(
                    usb_id = %identity.usb_id,
                    manufacturer = %identity.manufacturer,
                    "usb bootloader identity not yet matched, still waiting"
                );
                Err(())
            }
        })
        .map_err(|()| timeout_outcome(&initial_identity, &last_identity, path, reset_error))
    }
}

fn timeout_outcome(
    initial: &UsbIdentity,
    last_observed: &UsbIdentity,
    device: &Path,
    reset_error: Option<String>,
) -> UsbBootloaderError {
    if last_observed == initial {
        UsbBootloaderError::NoEffect {
            device: device.to_path_buf(),
        }
    } else {
        UsbBootloaderError::NotDetected {
            device: device.to_path_buf(),
            reset_error,
        }
    }
}

/// A failed USB Katapult bootloader transition.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UsbBootloaderError {
    /// The configured serial device could not be related to a USB device.
    #[error("{} is not a USB device", .device.display())]
    NotUsbDevice {
        /// The configured serial device.
        device: PathBuf,
    },
    /// Katapult did not appear at the same USB topology before the timeout.
    #[error(
        "no bootloader appeared at {} before the timeout{}",
        .device.display(),
        .reset_error.as_deref().map(|error| format!(" (reset error: {error})")).unwrap_or_default()
    )]
    NotDetected {
        /// The configured serial device.
        device: PathBuf,
        /// A best-effort reset error, if opening the device failed before it disconnected.
        reset_error: Option<String>,
    },
    /// The device never left its pre-reset identity before the timeout.
    #[error("{} never left application firmware; the reset request had no effect", .device.display())]
    NoEffect {
        /// The configured serial device or observed USB topology.
        device: PathBuf,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsbIdentity {
    usb_id: String,
    manufacturer: String,
}

impl UsbIdentity {
    fn is_complete(&self) -> bool {
        self.usb_id
            .split_once(':')
            .is_some_and(|(vendor, product)| !vendor.is_empty() && !product.is_empty())
    }
}

fn usb_device_path(device: &Path) -> Option<PathBuf> {
    let tty = fs::canonicalize(device).ok()?.file_name()?.to_owned();
    let tty_path = fs::canonicalize(Path::new("/sys/class/tty").join(tty)).ok()?;
    usb_device_ancestor(&tty_path)
}

fn usb_identity(usb_path: &Path) -> UsbIdentity {
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

fn usb_tty(usb_path: &Path) -> Option<PathBuf> {
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

impl SerialIo for SystemSerialIo {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.port.write_all(bytes)
    }

    fn read(&mut self) -> Result<Vec<u8>, Self::Error> {
        let mut buffer = [0_u8; 256];
        let count = self.port.read(&mut buffer)?;
        Ok(buffer[..count].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{SystemSerialIo, UsbBootloaderError, UsbIdentity};
    use std::fs;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn rejects_the_transient_empty_usb_identity_during_disconnect() {
        assert!(
            !UsbIdentity {
                usb_id: ":".to_owned(),
                manufacturer: String::new(),
            }
            .is_complete()
        );
        assert!(
            UsbIdentity {
                usb_id: "2e8a:0003".to_owned(),
                manufacturer: "raspberry pi".to_owned(),
            }
            .is_complete()
        );
    }

    #[test]
    fn waits_for_the_tty_to_appear_after_the_usb_identity_re_enumerates() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        write_identity(&usb_path, "1d50:6177", "openmoko, inc.");

        let watched_path = usb_path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            write_tty(&watched_path, "ttyACM0");
        });

        let observed = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            "2e8a:000c",
            "raspberry pi",
            Duration::from_millis(500),
            Duration::from_millis(5),
        )
        .expect("bootloader should eventually be observed");

        writer.join().expect("writer thread should not panic");
        assert_eq!(observed.serial_device, Some(PathBuf::from("/dev/ttyACM0")));

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    #[test]
    fn reports_no_effect_when_the_device_never_leaves_its_pre_reset_identity() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        write_identity(&usb_path, "1d50:6177", "openmoko, inc.");

        let error = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            "1d50:6177",
            "openmoko, inc.",
            Duration::from_millis(30),
            Duration::from_millis(5),
        )
        .expect_err("identity never changes, so the wait should time out");

        assert!(matches!(error, UsbBootloaderError::NoEffect { .. }));

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    #[test]
    fn reports_not_detected_when_a_new_identity_never_exposes_a_tty() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        write_identity(&usb_path, "2e8a:000c", "raspberry pi");

        let error = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            "1d50:6177",
            "openmoko, inc.",
            Duration::from_millis(30),
            Duration::from_millis(5),
        )
        .expect_err("no tty ever appears, so the wait should time out");

        assert!(matches!(error, UsbBootloaderError::NotDetected { .. }));

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    fn write_identity(usb_path: &std::path::Path, usb_id: &str, manufacturer: &str) {
        let (vendor, product) = usb_id.split_once(':').expect("usb id has vendor:product");
        fs::write(usb_path.join("idVendor"), format!("{vendor}\n")).expect("write idVendor");
        fs::write(usb_path.join("idProduct"), format!("{product}\n")).expect("write idProduct");
        fs::write(usb_path.join("manufacturer"), format!("{manufacturer}\n"))
            .expect("write manufacturer");
    }

    fn write_tty(usb_path: &std::path::Path, tty: &str) {
        let interface = usb_path.join(format!(
            "{}:1.0",
            usb_path.file_name().unwrap().to_string_lossy()
        ));
        let tty_dir = interface.join("tty").join(tty);
        fs::create_dir_all(tty_dir).expect("create tty fixture");
    }

    fn unique_temporary_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "aldis-katapult-serial-{}-{nonce}",
            std::process::id()
        ))
    }
}
