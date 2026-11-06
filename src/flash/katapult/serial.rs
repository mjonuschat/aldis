//! Katapult serial transport framing.
//!
//! The bootloader entry sequence and protocol framing follow
//! <https://github.com/Arksine/katapult/blob/master/scripts/flashtool.py>.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use std::fs;
use std::path::PathBuf;

use super::session::Transport;
use super::{AssemblyError, MAX_RESPONSE_FRAME_BYTES, ResponseAssembler};
use crate::flash::usb_bootloader::{ObservedUsbBootloader, UsbBootloaderKind};
use crate::flash::usb_sysfs::{
    UsbIdentity, UsbIdentitySettler, usb_device_ancestor, usb_identity, usb_tty,
};
use crate::retry::retry_until_available;

/// Klipper's explicit request to reboot a serial MCU into its bootloader.
pub const BOOTLOADER_ENTRY_REQUEST: &[u8] = b"~ \x1c Request Serial Bootloader!! ~";

/// Returns whether a re-enumerated serial bootloader USB identity is still missing its tty.
///
/// Katapult's and SAM-BA's CDC-ACM interfaces enumerate after their USB identity, so callers
/// must keep waiting rather than treat the bootloader as observed yet.
fn awaiting_serial_device(
    usb_id: &str,
    manufacturer: &str,
    serial_device: &Option<PathBuf>,
) -> bool {
    serial_device.is_none()
        && matches!(
            crate::flash::usb_bootloader::classify_usb_identity(usb_id, manufacturer),
            Some(UsbBootloaderKind::Katapult | UsbBootloaderKind::SamBa)
        )
}

/// Builds the observed bootloader at `usb_path`, unless it's a Katapult
/// identity still waiting on its CDC-ACM tty to enumerate.
fn observed_if_ready(usb_path: &Path, identity: &UsbIdentity) -> Option<ObservedUsbBootloader> {
    let serial_device = usb_tty(usb_path);
    if awaiting_serial_device(&identity.usb_id, &identity.manufacturer, &serial_device) {
        return None;
    }
    Some(ObservedUsbBootloader {
        sysfs_path: usb_path.to_path_buf(),
        usb_id: identity.usb_id.clone(),
        manufacturer: identity.manufacturer.clone(),
        serial_device,
    })
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
    /// No complete response arrived before the exchange deadline.
    Timeout,
    /// The serial device returned no bytes, which a tty does only once it has gone away.
    Disconnected,
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
            Self::Timeout => write!(
                f,
                "serial exchange did not complete within the configured timeout"
            ),
            Self::Disconnected => write!(f, "the serial device returned no data"),
        }
    }
}

impl std::error::Error for SerialTransportError<io::Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::ResponseTooLarge { .. }
            | Self::ResponseLengthExceeded { .. }
            | Self::Timeout
            | Self::Disconnected => None,
        }
    }
}

/// How long one [`KatapultSerialTransport`] exchange may take in total. Reads of a real port
/// each return within the port's read timeout, so this only matters for a device that keeps
/// sending bytes that never form a frame.
pub const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);

/// A sequential Katapult transport over serial bytes.
pub struct KatapultSerialTransport<T> {
    io: T,
    exchange_timeout: Duration,
}

impl<T> KatapultSerialTransport<T> {
    /// Creates a transport over an already-open serial I/O implementation with the
    /// [`DEFAULT_EXCHANGE_TIMEOUT`].
    pub fn new(io: T) -> Self {
        Self::with_timeout(io, DEFAULT_EXCHANGE_TIMEOUT)
    }

    /// Creates a transport whose every `exchange` is bounded by `exchange_timeout`. As for the
    /// CAN transport, the deadline is checked before each read, so the worst case is the timeout
    /// plus one read's own blocking time.
    pub fn with_timeout(io: T, exchange_timeout: Duration) -> Self {
        Self {
            io,
            exchange_timeout,
        }
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
        let deadline = Instant::now() + self.exchange_timeout;
        let mut assembler = ResponseAssembler::new();
        loop {
            if Instant::now() >= deadline {
                return Err(SerialTransportError::Timeout);
            }
            let chunk = self.io.read().map_err(SerialTransportError::Io)?;
            if chunk.is_empty() {
                return Err(SerialTransportError::Disconnected);
            }
            match assembler.push(&chunk) {
                Ok(Some(response)) => return Ok(response),
                Ok(None) => {}
                Err(AssemblyError::TooLarge { length }) => {
                    return Err(SerialTransportError::ResponseTooLarge { length });
                }
                Err(AssemblyError::LengthExceeded { expected, actual }) => {
                    return Err(SerialTransportError::ResponseLengthExceeded { expected, actual });
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

    /// Asks a running Klipper on a plain UART to reboot into Katapult.
    pub fn request_serial_bootloader(path: &Path, baud_rate: u32) -> serialport::Result<()> {
        let mut port = serialport::new(path.to_string_lossy(), baud_rate).open()?;
        port.write_all(BOOTLOADER_ENTRY_REQUEST)?;
        port.flush()?;
        Ok(())
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

    /// Requests USB bootloader entry and returns the observed bootloader at the same topology.
    pub fn request_and_observe_usb_bootloader(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Result<ObservedUsbBootloader, UsbBootloaderError> {
        Self::request_usb_bootloader_and_wait(
            path,
            timeout,
            poll_interval,
            matches,
            observed_if_ready,
        )
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
        wait_for_settled_identity(
            &initial_identity,
            timeout,
            poll_interval,
            || usb_identity(usb_path),
            |identity| observed_if_ready(usb_path, identity),
        )
        .map_err(|last_identity| timeout_outcome(&initial_identity, &last_identity, usb_path, None))
    }

    fn request_usb_bootloader_and_wait<T>(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
        matches: impl Fn(&str, &str) -> bool,
        ready: impl Fn(&Path, &UsbIdentity) -> Option<T>,
    ) -> Result<T, UsbBootloaderError> {
        let usb_path = usb_device_path(path).ok_or_else(|| UsbBootloaderError::NotUsbDevice {
            device: path.to_path_buf(),
        })?;
        let initial_identity = usb_identity(&usb_path);
        let reset_error = Self::request_usb_bootloader(path)
            .err()
            .map(|error| error.to_string());

        wait_for_settled_identity(
            &initial_identity,
            timeout,
            poll_interval,
            || usb_identity(&usb_path),
            |identity| {
                matches(&identity.usb_id, &identity.manufacturer)
                    .then(|| ready(&usb_path, identity))
                    .flatten()
            },
        )
        .map_err(|last_identity| {
            timeout_outcome(&initial_identity, &last_identity, path, reset_error)
        })
    }
}

/// Polls `read_identity` until the device settles on an identity other than `initial` and
/// `accept` produces a result for it; on timeout returns the last identity read.
///
/// Every read passes through [`UsbIdentitySettler`], so a single sysfs read taken mid
/// re-enumeration is never mistaken for the device's new identity. The reader and the
/// acceptance step are injected so tests can script the exact read sequence.
fn wait_for_settled_identity<T>(
    initial: &UsbIdentity,
    timeout: Duration,
    poll_interval: Duration,
    mut read_identity: impl FnMut() -> UsbIdentity,
    mut accept: impl FnMut(&UsbIdentity) -> Option<T>,
) -> Result<T, UsbIdentity> {
    let mut settler = UsbIdentitySettler::new(initial.clone());
    let mut last_identity = initial.clone();
    retry_until_available(timeout, poll_interval, || {
        let identity = read_identity();
        last_identity = identity.clone();
        if let Some(settled) = settler.observe(&identity)
            && let Some(result) = accept(settled)
        {
            Ok(result)
        } else {
            tracing::debug!(
                usb_id = %identity.usb_id,
                manufacturer = %identity.manufacturer,
                "usb bootloader not yet observed, still waiting"
            );
            Err(())
        }
    })
    .map_err(|()| last_identity)
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
            reset_error,
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
    /// No usable bootloader was found at the same USB topology before the timeout: either the
    /// identity changed to something unrecognised, or a recognised one had no usable endpoint yet.
    #[error(
        "no usable bootloader was found at {} before the timeout{}",
        .device.display(),
        .reset_error.as_deref().map(|error| format!(" (the serial request reported: {error})")).unwrap_or_default()
    )]
    NotDetected {
        /// The configured serial device.
        device: PathBuf,
        /// A best-effort reset error, if opening the device failed before it disconnected.
        reset_error: Option<String>,
    },
    /// The device never left its pre-reset identity before the timeout.
    #[error(
        "{} still had its application's USB identity when the bootloader wait timed out{}",
        .device.display(),
        .reset_error.as_deref().map(|error| format!(" (the serial request reported: {error})")).unwrap_or_default()
    )]
    NoEffect {
        /// The configured serial device or observed USB topology.
        device: PathBuf,
        /// A best-effort reset error, if opening the device failed before the wait began.
        reset_error: Option<String>,
    },
}

const TTY_SYSFS_ROOT: &str = "/sys/class/tty";

fn tty_sysfs_path(tty_root: &Path, device: &Path) -> Option<PathBuf> {
    let tty = fs::canonicalize(device).ok()?.file_name()?.to_owned();
    fs::canonicalize(tty_root.join(tty)).ok()
}

fn usb_device_path(device: &Path) -> Option<PathBuf> {
    usb_device_ancestor(&tty_sysfs_path(Path::new(TTY_SYSFS_ROOT), device)?)
}

/// Returns whether `device` is a real tty with no USB device behind it, such as a Pi's UART.
pub fn is_tty_without_usb_device(device: &Path) -> bool {
    is_tty_without_usb_device_in(Path::new(TTY_SYSFS_ROOT), device)
}

fn is_tty_without_usb_device_in(tty_root: &Path, device: &Path) -> bool {
    tty_sysfs_path(tty_root, device).is_some_and(|tty| usb_device_ancestor(&tty).is_none())
}

impl SerialIo for SystemSerialIo {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.port.write_all(bytes)
    }

    fn read(&mut self) -> Result<Vec<u8>, Self::Error> {
        let mut buffer = [0_u8; 256];
        let count = self.port.read(&mut buffer)?;
        if count == 0 {
            // A tty read returns zero bytes only at end of file, i.e. once the device is gone.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the serial device returned no data",
            ));
        }
        Ok(buffer[..count].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{SystemSerialIo, UsbBootloaderError, UsbIdentity, wait_for_settled_identity};
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
    fn reports_not_detected_when_a_new_katapult_identity_never_exposes_a_tty() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        write_identity(&usb_path, "1d50:6177", "katapult");

        let error = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            "2e8a:000c",
            "raspberry pi",
            Duration::from_millis(30),
            Duration::from_millis(5),
        )
        .expect_err("katapult never exposes a tty, so the wait should time out");

        assert!(matches!(error, UsbBootloaderError::NotDetected { .. }));

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    #[test]
    fn observes_a_non_katapult_bootloader_immediately_without_waiting_for_a_tty() {
        let usb_path = unique_temporary_path();
        fs::create_dir_all(&usb_path).expect("create sysfs fixture");
        write_identity(&usb_path, "0483:df11", "stmicroelectronics");

        let observed = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            "1d50:6177",
            "openmoko, inc.",
            Duration::from_millis(500),
            Duration::from_millis(5),
        )
        .expect("stm32 dfu should be observed without a serial device");

        assert_eq!(observed.usb_id, "0483:df11");
        assert_eq!(observed.serial_device, None);

        fs::remove_dir_all(usb_path).expect("remove sysfs fixture");
    }

    fn id(usb_id: &str, manufacturer: &str) -> UsbIdentity {
        UsbIdentity {
            usb_id: usb_id.to_owned(),
            manufacturer: manufacturer.to_owned(),
        }
    }

    /// Replays `script` one read per call, repeating its last entry forever.
    fn scripted(script: Vec<UsbIdentity>) -> impl FnMut() -> UsbIdentity {
        let mut reads = 0;
        move || {
            let identity = script[reads.min(script.len() - 1)].clone();
            reads += 1;
            identity
        }
    }

    fn wait(
        script: Vec<UsbIdentity>,
        accept: impl FnMut(&UsbIdentity) -> Option<String>,
    ) -> Result<String, UsbIdentity> {
        wait_for_settled_identity(
            &id("1d50:606f", "klipper"),
            Duration::from_secs(2),
            Duration::from_millis(1),
            scripted(script),
            accept,
        )
    }

    fn accept_any(identity: &UsbIdentity) -> Option<String> {
        Some(identity.usb_id.clone())
    }

    #[test]
    fn a_one_read_manufacturer_loss_is_not_taken_for_the_bootloader() {
        let script = vec![
            id("1d50:606f", "klipper"),
            id("1d50:606f", ""),
            id("1d50:6177", "katapult"),
        ];

        assert_eq!(wait(script, accept_any), Ok("1d50:6177".to_owned()));
    }

    #[test]
    fn a_brief_manufacturer_loss_is_tolerated() {
        let mut script = vec![id("1d50:606f", "klipper")];
        script.extend(vec![id("1d50:606f", ""); 4]);
        script.push(id("1d50:6177", "katapult"));

        assert_eq!(wait(script, accept_any), Ok("1d50:6177".to_owned()));
    }

    #[test]
    fn a_sustained_manufacturer_loss_is_not_taken_for_the_bootloader() {
        let mut script = vec![id("1d50:606f", "klipper")];
        script.extend(vec![id("1d50:606f", ""); 30]);
        script.push(id("1d50:6177", "katapult"));

        assert_eq!(wait(script, accept_any), Ok("1d50:6177".to_owned()));
    }

    #[test]
    fn empty_reads_between_the_two_identities_never_settle() {
        let script = vec![
            id("1d50:606f", "klipper"),
            id(":", ""),
            id(":", ""),
            id("1d50:6177", "katapult"),
        ];

        assert_eq!(wait(script, accept_any), Ok("1d50:6177".to_owned()));
    }

    #[test]
    fn keeps_waiting_while_the_settled_identity_is_not_yet_accepted() {
        let mut rejections = 3;
        let result = wait(vec![id("1d50:6177", "katapult")], |identity| {
            if rejections > 0 {
                rejections -= 1;
                None
            } else {
                Some(identity.usb_id.clone())
            }
        });

        assert_eq!(result, Ok("1d50:6177".to_owned()));
        assert_eq!(rejections, 0);
    }

    #[test]
    fn times_out_with_the_last_identity_when_the_device_never_changes() {
        let result = wait_for_settled_identity(
            &id("1d50:606f", "klipper"),
            Duration::from_millis(50),
            Duration::from_millis(1),
            scripted(vec![id("1d50:606f", "klipper")]),
            accept_any,
        );

        assert_eq!(result, Err(id("1d50:606f", "klipper")));
    }

    #[test]
    fn rejects_an_identity_the_caller_does_not_accept() {
        let result = wait_for_settled_identity(
            &id("1d50:606f", "klipper"),
            Duration::from_millis(50),
            Duration::from_millis(1),
            scripted(vec![id("1d50:614e", "klipper")]),
            |_| None::<String>,
        );

        assert_eq!(result, Err(id("1d50:614e", "klipper")));
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

    fn tty_fixture(root: &std::path::Path, sysfs_leaf: &str, tty: &str) -> (PathBuf, PathBuf) {
        let leaf = root.join("devices").join(sysfs_leaf).join(tty);
        fs::create_dir_all(&leaf).expect("create sysfs leaf");
        let tty_root = root.join("class-tty");
        fs::create_dir_all(&tty_root).expect("create tty class root");
        std::os::unix::fs::symlink(&leaf, tty_root.join(tty)).expect("link tty class entry");
        let device = root.join("dev").join(tty);
        fs::create_dir_all(device.parent().expect("device has a parent")).expect("create dev");
        fs::write(&device, "").expect("create device node stand-in");
        (tty_root, device)
    }

    #[test]
    fn treats_a_tty_without_a_usb_ancestor_as_a_uart() {
        let root = unique_temporary_path();
        let (tty_root, device) = tty_fixture(&root, "platform/serial-5", "ttyAMA5");

        assert!(super::is_tty_without_usb_device_in(&tty_root, &device));

        fs::remove_dir_all(root).expect("remove sysfs fixture");
    }

    #[test]
    fn does_not_treat_a_usb_backed_tty_as_a_uart() {
        let root = unique_temporary_path();
        let (tty_root, device) = tty_fixture(&root, "usb1/1-1/1-1:1.0/tty", "ttyACM0");
        let usb_device = root.join("devices").join("usb1").join("1-1");
        for (name, value) in [
            ("idVendor", "1d50"),
            ("idProduct", "614e"),
            ("busnum", "1"),
            ("devnum", "5"),
        ] {
            fs::write(usb_device.join(name), value).expect("write usb identity");
        }

        assert!(!super::is_tty_without_usb_device_in(&tty_root, &device));

        fs::remove_dir_all(root).expect("remove sysfs fixture");
    }

    #[test]
    fn does_not_treat_an_unresolvable_device_as_a_uart() {
        let root = unique_temporary_path();

        assert!(!super::is_tty_without_usb_device_in(
            &root.join("class-tty"),
            &root.join("dev").join("ttyAMA9")
        ));
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
