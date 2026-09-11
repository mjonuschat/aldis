//! Katapult serial transport framing.
//!
//! The bootloader entry sequence and protocol framing follow
//! <https://github.com/Arksine/katapult/blob/master/scripts/flashtool.py>.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Instant;

use super::MAX_RESPONSE_FRAME_BYTES;
use super::session::Transport;

/// Klipper's explicit request to reboot a serial MCU into its bootloader.
pub const BOOTLOADER_ENTRY_REQUEST: &[u8] = b"~ \x1c Request Serial Bootloader!! ~";

const KATAPULT_USB_ID: &str = "1d50:6177";

/// Returns whether USB identity fields identify a Katapult bootloader.
pub fn is_katapult_usb(usb_id: &str, manufacturer: &str) -> bool {
    usb_id.eq_ignore_ascii_case(KATAPULT_USB_ID) || manufacturer.eq_ignore_ascii_case("katapult")
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

    /// Requests USB bootloader entry and returns its re-enumerated sysfs device path.
    ///
    /// The path identifies the same physical USB topology as the running serial
    /// device. Native USB bootloader backends use it to avoid selecting an
    /// unrelated device with the same vendor and product identifiers.
    pub fn request_and_find_usb_device(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Result<PathBuf, UsbBootloaderError> {
        Self::request_usb_bootloader_and_wait(path, timeout, poll_interval, matches, |usb_path| {
            Some(usb_path.to_path_buf())
        })
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
        let deadline = Instant::now() + timeout;
        let interval = poll_interval.max(Duration::from_millis(10));

        loop {
            let identity = usb_identity(&usb_path);
            if identity != initial_identity
                && matches(&identity.usb_id, &identity.manufacturer)
                && let Some(result) = ready(&usb_path)
            {
                return Ok(result);
            }
            if Instant::now() >= deadline {
                return Err(UsbBootloaderError::NotDetected {
                    device: path.to_path_buf(),
                    reset_error,
                });
            }
            thread::sleep(interval);
        }
    }
}

/// A failed USB Katapult bootloader transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsbBootloaderError {
    /// The configured serial device could not be related to a USB device.
    NotUsbDevice {
        /// The configured serial device.
        device: PathBuf,
    },
    /// Katapult did not appear at the same USB topology before the timeout.
    NotDetected {
        /// The configured serial device.
        device: PathBuf,
        /// A best-effort reset error, if opening the device failed before it disconnected.
        reset_error: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsbIdentity {
    usb_id: String,
    manufacturer: String,
}

fn usb_device_path(device: &Path) -> Option<PathBuf> {
    let tty = fs::canonicalize(device).ok()?.file_name()?.to_owned();
    let tty_path = fs::canonicalize(Path::new("/sys/class/tty").join(tty)).ok()?;
    tty_path.ancestors().find_map(|candidate| {
        let has_usb_identity =
            candidate.join("idVendor").is_file() && candidate.join("idProduct").is_file();
        (has_usb_identity
            && candidate.join("busnum").is_file()
            && candidate.join("devnum").is_file())
        .then(|| candidate.to_path_buf())
    })
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
