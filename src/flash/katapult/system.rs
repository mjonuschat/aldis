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
    /// Per-read serial or SocketCAN timeout.
    pub read_timeout: Duration,
    /// Delay between a CAN reboot request and temporary-node assignment.
    pub can_bootloader_settle: Duration,
}

/// A failure while selecting, bootstrapping, or flashing Katapult on Linux.
#[derive(Debug)]
pub enum SystemKatapultError {
    /// The prepared target has no supported transport.
    Endpoint(EndpointError),
    /// Serial bootloader transition failed.
    Serial(SerialBootstrapError),
    /// SocketCAN could not be opened.
    CanSocket(io::Error),
    /// CAN bootstrap transition failed.
    Can(CanBootstrapError<io::Error>),
    /// Katapult rejected or could not verify the transfer.
    Flash(KatapultFlashError),
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
