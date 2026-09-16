//! Linux system adapter for an explicitly approved Katapult flash.

use std::io;
use std::thread;
use std::time::Duration;

use crate::flash::{FlashPort, FlashResult};
use crate::prepare::PreparedBuild;

use super::adapter::KatapultFlashError;
use super::bootstrap::{
    CanBootstrapError, SerialBootstrapError, bootstrap_system_serial, request_can_bootloader,
};
use super::can::SocketCanIo;
use super::endpoint::{EndpointError, KatapultEndpoint, endpoint_for};

/// Timing and baud settings for a native Katapult flash.
#[derive(Clone, Copy, Debug)]
pub struct SystemKatapultOptions {
    /// Katapult serial baud rate.
    pub baud_rate: u32,
    /// Maximum time to wait for USB Katapult re-enumeration.
    pub bootloader_timeout: Duration,
    /// Poll interval while watching USB topology.
    pub poll_interval: Duration,
    /// Per-attempt serial or SocketCAN response timeout.
    ///
    /// Katapult's own `flashtool.py` waits up to 5s per `SEND_BLOCK` attempt
    /// because a block write can trigger a flash sector erase; since aldis
    /// doesn't vary the timeout per command, it must be at least that long
    /// for every command, not just the fast ones like `CONNECT`.
    pub read_timeout: Duration,
    /// Delay between a CAN reboot request and temporary-node assignment.
    pub can_bootloader_settle: Duration,
}

/// A failure while selecting, bootstrapping, or flashing Katapult on Linux.
#[derive(Debug, thiserror::Error)]
pub enum SystemKatapultError {
    /// The prepared target has no supported transport.
    #[error("no supported Katapult transport: {0:?}")]
    Endpoint(EndpointError),
    /// Serial bootloader transition failed.
    #[error("serial bootloader transition failed: {0:?}")]
    Serial(SerialBootstrapError),
    /// SocketCAN could not be opened.
    #[error("could not open SocketCAN interface: {0}")]
    CanSocket(#[source] io::Error),
    /// CAN bootstrap transition failed.
    #[error("CAN bootstrap transition failed: {0}")]
    Can(#[source] CanBootstrapError<io::Error>),
    /// Katapult rejected or could not verify the transfer.
    #[error("Katapult flash failed: {0}")]
    Flash(#[source] KatapultFlashError),
}

/// Flashes one prepared target after the caller has stopped Klipper.
pub fn flash_system(
    prepared: &PreparedBuild,
    firmware: &[u8],
    options: SystemKatapultOptions,
) -> Result<FlashResult, SystemKatapultError> {
    match endpoint_for(prepared).map_err(SystemKatapultError::Endpoint)? {
        KatapultEndpoint::Serial { running_device } => {
            let mut backend = bootstrap_system_serial(
                &running_device,
                options.baud_rate,
                options.bootloader_timeout,
                options.poll_interval,
                options.read_timeout,
            )
            .map_err(SystemKatapultError::Serial)?;
            backend.flash(firmware).map_err(SystemKatapultError::Flash)
        }
        KatapultEndpoint::Can { interface, uuid } => {
            let io = SocketCanIo::open(&interface, options.read_timeout)
                .map_err(SystemKatapultError::CanSocket)?;
            let bootstrap = request_can_bootloader(io, uuid).map_err(SystemKatapultError::Can)?;
            thread::sleep(options.can_bootloader_settle);
            let mut backend = bootstrap.connect().map_err(SystemKatapultError::Can)?;
            backend.flash(firmware).map_err(SystemKatapultError::Flash)
        }
    }
}
