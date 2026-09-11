//! Explicit transitions from a running MCU transport to a ready Katapult backend.

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::Duration;

use super::backend::KatapultBackend;
use super::can::{CanError, CanIo, CanTransportError, KatapultCanAddress, KatapultCanTransport};
#[cfg(target_os = "linux")]
use super::serial::{KatapultSerialTransport, SystemSerialIo, UsbBootloaderError};

/// A failure while transitioning a CAN MCU into a ready Katapult session.
#[derive(Debug)]
pub enum CanBootstrapError<E> {
    /// The configured CAN UUID cannot be represented by Katapult.
    Address(CanError),
    /// The CAN reboot or node-assignment command failed.
    Transport(CanTransportError<E>),
}

/// A failure while creating a ready serial Katapult backend.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum SerialBootstrapError {
    /// The running USB device did not re-enumerate as Katapult.
    Bootloader(UsbBootloaderError),
    /// The detected Katapult serial device could not be opened.
    Open(serialport::Error),
}

/// Enters Katapult on a USB serial target and opens a ready serial backend.
///
/// This must only be called after the update coordinator has stopped Klipper.
/// It performs a 1200-baud reset, waits for Katapult at the same USB topology,
/// and opens the detected bootloader tty at `baud_rate`.
#[cfg(target_os = "linux")]
pub fn bootstrap_system_serial(
    running_device: &Path,
    baud_rate: u32,
    bootloader_timeout: Duration,
    poll_interval: Duration,
    read_timeout: Duration,
) -> Result<KatapultBackend<KatapultSerialTransport<SystemSerialIo>>, SerialBootstrapError> {
    let bootloader_device = SystemSerialIo::request_and_find_usb_bootloader(
        running_device,
        bootloader_timeout,
        poll_interval,
    )
    .map_err(SerialBootstrapError::Bootloader)?;
    let io = SystemSerialIo::open(&bootloader_device, baud_rate, read_timeout)
        .map_err(SerialBootstrapError::Open)?;
    Ok(KatapultBackend::new(KatapultSerialTransport::new(io)))
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
    ) -> Result<KatapultBackend<KatapultCanTransport<T>>, CanBootstrapError<T::Error>> {
        self.transport
            .assign_node()
            .map_err(CanBootstrapError::Transport)?;
        Ok(KatapultBackend::for_canbus(self.transport, self.uuid))
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
