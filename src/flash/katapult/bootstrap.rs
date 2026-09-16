//! Explicit transitions from a running MCU transport to a ready Katapult backend.

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use super::adapter::KatapultAdapter;
use super::can::{CanError, CanIo, CanTransportError, KatapultCanAddress, KatapultCanTransport};
use super::serial::{KatapultSerialTransport, SystemSerialIo, UsbBootloaderError};
use crate::flash::usb_bootloader::{
    SelectedUsbBootloader, UsbBootloaderSelectionError, select_usb_bootloader,
};

/// A failure while transitioning a CAN MCU into a ready Katapult session.
#[derive(Debug)]
pub enum CanBootstrapError<E> {
    /// The configured CAN UUID cannot be represented by Katapult.
    Address(CanError),
    /// The CAN reboot or node-assignment command failed.
    Transport(CanTransportError<E>),
}

impl fmt::Display for CanBootstrapError<io::Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(error) => write!(f, "{error}"),
            Self::Transport(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CanBootstrapError<io::Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Address(error) => error.source(),
            Self::Transport(error) => error.source(),
        }
    }
}

/// A failure while creating a ready serial Katapult backend.
#[derive(Debug, thiserror::Error)]
pub enum SerialBootstrapError {
    /// The running USB device did not re-enumerate as Katapult.
    #[error("running USB device did not re-enumerate as Katapult: {0}")]
    Bootloader(#[source] UsbBootloaderError),
    /// The observed bootloader is unsupported or cannot be used safely.
    #[error("observed bootloader is unsupported or cannot be used safely: {0}")]
    Selection(#[source] UsbBootloaderSelectionError),
    /// A supported but non-Katapult bootloader appeared at the selected topology.
    #[error("expected a Katapult bootloader, found {0:?} at the selected topology")]
    UnexpectedBootloader(SelectedUsbBootloader),
    /// The detected Katapult serial device could not be opened.
    #[error(transparent)]
    Open(serialport::Error),
}

/// Enters Katapult on a USB serial target and opens a ready serial backend.
///
/// This must only be called after the update coordinator has stopped Klipper.
/// It performs a 1200-baud reset, waits for Katapult at the same USB topology,
/// and opens the detected bootloader tty at `baud_rate`.
pub fn bootstrap_system_serial(
    running_device: &Path,
    baud_rate: u32,
    bootloader_timeout: Duration,
    poll_interval: Duration,
    read_timeout: Duration,
) -> Result<KatapultAdapter<KatapultSerialTransport<SystemSerialIo>>, SerialBootstrapError> {
    let bootloader = SystemSerialIo::request_and_observe_any_usb_bootloader(
        running_device,
        bootloader_timeout,
        poll_interval,
    )
    .map_err(SerialBootstrapError::Bootloader)?;
    let bootloader_device =
        match select_usb_bootloader(bootloader).map_err(SerialBootstrapError::Selection)? {
            SelectedUsbBootloader::Katapult { serial_device, .. } => serial_device,
            bootloader => return Err(SerialBootstrapError::UnexpectedBootloader(bootloader)),
        };
    let io = SystemSerialIo::open(&bootloader_device, baud_rate, read_timeout)
        .map_err(SerialBootstrapError::Open)?;
    Ok(KatapultAdapter::new(KatapultSerialTransport::new(io)))
}

/// A CAN target that has been asked to enter Katapult but is not yet connected.
///
/// The caller is responsible for waiting for the bootloader to start before
/// calling [`Self::connect`]. This prevents a timing assumption from being
/// hidden inside the transport factory.
pub struct CanKatapultBootstrap<T> {
    transport: KatapultCanTransport<T>,
    uuid: u64,
}

impl<T> CanKatapultBootstrap<T> {
    /// Returns the transport without assigning Katapult's temporary node ID.
    pub fn into_transport(self) -> KatapultCanTransport<T> {
        self.transport
    }
}

impl<T: CanIo> CanKatapultBootstrap<T> {
    /// Assigns Katapult's temporary node ID and returns a ready flashing backend.
    pub fn connect(
        mut self,
    ) -> Result<KatapultAdapter<KatapultCanTransport<T>>, CanBootstrapError<T::Error>> {
        self.transport
            .assign_node()
            .map_err(CanBootstrapError::Transport)?;
        Ok(KatapultAdapter::for_canbus(self.transport, self.uuid))
    }
}

/// Requests bootloader entry for a configured CAN UUID without assigning a node.
pub fn request_can_bootloader<T: CanIo>(
    io: T,
    uuid: u64,
) -> Result<CanKatapultBootstrap<T>, CanBootstrapError<T::Error>> {
    let address = KatapultCanAddress::new(uuid).map_err(CanBootstrapError::Address)?;
    let mut transport = KatapultCanTransport::new(io, address);
    transport
        .request_bootloader_entry()
        .map_err(CanBootstrapError::Transport)?;
    Ok(CanKatapultBootstrap { transport, uuid })
}
