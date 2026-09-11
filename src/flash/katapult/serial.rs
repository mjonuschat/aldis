//! Katapult serial transport framing.
//!
//! The bootloader entry sequence and protocol framing follow
//! <https://github.com/Arksine/katapult/blob/master/scripts/flashtool.py>.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use super::MAX_RESPONSE_FRAME_BYTES;
use super::session::Transport;

/// Klipper's explicit request to reboot a serial MCU into its bootloader.
pub const BOOTLOADER_ENTRY_REQUEST: &[u8] = b"~ \x1c Request Serial Bootloader!! ~";

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
