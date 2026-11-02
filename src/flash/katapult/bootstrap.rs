//! Explicit transitions from a running MCU transport to a ready Katapult backend.

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use super::adapter::KatapultAdapter;
use super::can::{CanError, CanIo, CanTransportError, KatapultCanAddress, KatapultCanTransport};
use super::serial::{KatapultSerialTransport, SystemSerialIo};
use super::session::{KatapultSession, SessionError, Transport};

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

/// Confirms Katapult answers on `transport`, sending nothing but its connect command.
pub fn confirm_bootloader<T: Transport>(transport: T) -> Result<KatapultAdapter<T>, SessionError>
where
    T::Error: fmt::Debug,
{
    let mut session = KatapultSession::new(transport);
    session.connect()?;
    Ok(KatapultAdapter::new(session.into_transport()))
}

/// A failure while leaving an already-detected Katapult bootloader.
#[derive(Debug, thiserror::Error)]
pub enum SerialLeaveError {
    /// The detected Katapult serial device could not be opened.
    #[error(transparent)]
    Open(serialport::Error),
    /// Katapult rejected the connect or complete exchange.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Exits an already-running Katapult bootloader without flashing new firmware.
///
/// This does not request bootloader entry or wait for re-enumeration: it assumes `serial_device` already is a
/// Katapult bootloader (as found by scanning the USB bus) and asks it to
/// start its application, the same command sent after a normal update
/// finishes.
pub fn leave_serial(
    serial_device: &Path,
    baud_rate: u32,
    read_timeout: Duration,
) -> Result<(), SerialLeaveError> {
    let io = SystemSerialIo::open(serial_device, baud_rate, read_timeout)
        .map_err(SerialLeaveError::Open)?;
    let mut session = KatapultSession::new(KatapultSerialTransport::new(io));
    session.connect()?;
    session.complete()?;
    Ok(())
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
    exchange_timeout: Duration,
) -> Result<CanKatapultBootstrap<T>, CanBootstrapError<T::Error>> {
    let address = KatapultCanAddress::new(uuid).map_err(CanBootstrapError::Address)?;
    let mut transport = KatapultCanTransport::new(io, address, exchange_timeout);
    transport
        .request_bootloader_entry()
        .map_err(CanBootstrapError::Transport)?;
    Ok(CanKatapultBootstrap { transport, uuid })
}
