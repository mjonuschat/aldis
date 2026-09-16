//! Validated Katapult endpoints derived from a prepared MCU update.

use std::path::PathBuf;

use crate::moonraker::McuTransport;
use crate::prepare::PreparedBuild;

/// The explicit host endpoint used for a Katapult bootloader transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KatapultEndpoint {
    /// The serial device currently used by the running Klipper application.
    ///
    /// The Katapult bootloader device path is intentionally not inferred from
    /// this path; it must be supplied after the serial device re-enumerates.
    Serial {
        /// The configured serial device for the running application.
        running_device: PathBuf,
    },
    /// The configured CAN interface and Katapult CAN identity.
    Can {
        /// The SocketCAN interface.
        interface: String,
        /// The configured six-byte Katapult UUID.
        uuid: u64,
    },
}

/// A prepared MCU cannot be used with the Katapult backend.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EndpointError {
    /// Moonraker reported neither a serial nor CAN transport for the target.
    #[error("moonraker reported neither a serial nor CAN transport for {target_name}")]
    MissingTransport {
        /// Moonraker's MCU object name.
        target_name: String,
    },
}

/// Converts revalidated Moonraker transport data into a Katapult endpoint.
pub fn endpoint_for(prepared: &PreparedBuild) -> Result<KatapultEndpoint, EndpointError> {
    match prepared.transport.as_ref() {
        Some(McuTransport::Serial { device }) => Ok(KatapultEndpoint::Serial {
            running_device: PathBuf::from(device),
        }),
        Some(McuTransport::Can { interface, uuid }) => Ok(KatapultEndpoint::Can {
            interface: interface.clone(),
            uuid: *uuid,
        }),
        None => Err(EndpointError::MissingTransport {
            target_name: prepared.target_name.clone(),
        }),
    }
}
