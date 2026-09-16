//! Katapult CAN addressing, framing, and transport assembly.
//!
//! Wire mapping follows Katapult's CAN flasher implementation:
//! <https://github.com/Arksine/katapult/blob/master/scripts/flashtool.py>.

use std::fmt;
use std::io;
use std::time::Duration;

use super::MAX_RESPONSE_FRAME_BYTES;
use super::session::Transport;
use socketcan::{CanDataFrame, CanSocket, EmbeddedFrame, Id, Socket};

const CAN_ADMIN_ID: u16 = 0x3f0;
const CAN_ADMIN_SET_NODE_ID: u8 = 0x11;
const KLIPPER_ADMIN_REBOOT: u8 = 0x02;
const KATAPULT_NODE_ID: u16 = 129;
const KATAPULT_REQUEST_ID: u16 = KATAPULT_NODE_ID * 2 + 0x100;
const KATAPULT_RESPONSE_ID: u16 = KATAPULT_REQUEST_ID + 1;
const CLASSIC_CAN_MAX_DATA: usize = 8;
const MAX_CAN_UUID: u64 = (1 << 48) - 1;

/// A classic CAN frame used by the Katapult transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanFrame {
    id: u16,
    data: Vec<u8>,
}

/// An invalid classic CAN frame or Katapult CAN address.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CanError {
    /// The identifier does not fit a standard 11-bit CAN frame.
    #[error("CAN identifier {id:#x} does not fit a standard 11-bit CAN frame")]
    IdentifierOutOfRange {
        /// The rejected identifier.
        id: u16,
    },
    /// The payload exceeds classic CAN's eight-byte data limit.
    #[error("CAN payload of {length} bytes exceeds the classic CAN limit of 8 bytes")]
    PayloadTooLarge {
        /// The rejected payload length.
        length: usize,
    },
    /// The supplied UUID does not fit Katapult's six-byte CAN UUID field.
    #[error("CAN UUID {uuid:#x} does not fit Katapult's six-byte CAN UUID field")]
    UuidOutOfRange {
        /// The rejected UUID.
        uuid: u64,
    },
}

/// Sends and receives classic CAN frames.
///
/// Implementations must apply a read timeout so unrelated CAN traffic cannot
/// make a Katapult request wait indefinitely.
pub trait CanIo {
    /// The CAN implementation's error type.
    type Error;

    /// Sends one classic CAN frame.
    fn write(&mut self, frame: CanFrame) -> Result<(), Self::Error>;

    /// Receives one classic CAN frame.
    fn read(&mut self) -> Result<CanFrame, Self::Error>;
}

/// An error while exchanging Katapult protocol frames over CAN.
#[derive(Debug)]
pub enum CanTransportError<E> {
    /// The underlying CAN implementation failed.
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

impl fmt::Display for CanTransportError<io::Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "CAN transport error"),
            Self::ResponseTooLarge { length } => write!(
                f,
                "CAN response of {length} bytes exceeds the maximum {MAX_RESPONSE_FRAME_BYTES} bytes"
            ),
            Self::ResponseLengthExceeded { expected, actual } => write!(
                f,
                "CAN response of {actual} bytes exceeds the {expected} bytes declared by its header"
            ),
        }
    }
}

impl std::error::Error for CanTransportError<io::Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::ResponseTooLarge { .. } | Self::ResponseLengthExceeded { .. } => None,
        }
    }
}

impl CanFrame {
    /// Creates a standard 11-bit CAN data frame.
    pub fn new(id: u16, data: &[u8]) -> Result<Self, CanError> {
        if id > 0x7ff {
            return Err(CanError::IdentifierOutOfRange { id });
        }
        if data.len() > CLASSIC_CAN_MAX_DATA {
            return Err(CanError::PayloadTooLarge { length: data.len() });
        }
        Ok(Self {
            id,
            data: data.to_vec(),
        })
    }

    /// Returns the standard CAN identifier.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// Returns the frame payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Katapult's temporary classic-CAN routing for one explicitly supplied UUID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KatapultCanAddress {
    uuid: u64,
}

impl KatapultCanAddress {
    /// Creates an address for a six-byte Katapult CAN UUID.
    pub fn new(uuid: u64) -> Result<Self, CanError> {
        if uuid > MAX_CAN_UUID {
            return Err(CanError::UuidOutOfRange { uuid });
        }
        tracing::debug!(
            uuid = %format_args!("{uuid:#x}"),
            node_id = KATAPULT_NODE_ID,
            "katapult can address resolved"
        );
        Ok(Self { uuid })
    }

    /// Returns the CAN-admin command that assigns Katapult its temporary node ID.
    pub fn assignment_frame(self) -> CanFrame {
        let mut data = [0_u8; CLASSIC_CAN_MAX_DATA];
        data[0] = CAN_ADMIN_SET_NODE_ID;
        data[1..7].copy_from_slice(&self.uuid.to_be_bytes()[2..]);
        data[7] = KATAPULT_NODE_ID as u8;
        CanFrame::new(CAN_ADMIN_ID, &data).expect("fixed Katapult frame is valid")
    }

    /// Returns the Klipper CAN-admin command that enters Katapult.
    pub fn bootloader_entry_frame(self) -> CanFrame {
        let mut data = [0_u8; 7];
        data[0] = KLIPPER_ADMIN_REBOOT;
        data[1..].copy_from_slice(&self.uuid.to_be_bytes()[2..]);
        CanFrame::new(CAN_ADMIN_ID, &data).expect("fixed Klipper CAN-admin frame is valid")
    }

    /// Returns the CAN identifier for host-to-Katapult protocol fragments.
    pub fn request_id(self) -> u16 {
        KATAPULT_REQUEST_ID
    }

    /// Returns the CAN identifier for Katapult-to-host protocol fragments.
    pub fn response_id(self) -> u16 {
        KATAPULT_RESPONSE_ID
    }

    /// Splits one complete Katapult protocol frame into classic CAN fragments.
    pub fn fragment_request(self, request: &[u8]) -> Vec<CanFrame> {
        request
            .chunks(CLASSIC_CAN_MAX_DATA)
            .map(|chunk| {
                CanFrame::new(self.request_id(), chunk)
                    .expect("Katapult request fragments fit classic CAN")
            })
            .collect()
    }
}

/// A sequential Katapult protocol transport over classic CAN.
pub struct KatapultCanTransport<T> {
    io: T,
    address: KatapultCanAddress,
}

impl<T> KatapultCanTransport<T> {
    /// Creates a transport without sending a CAN-admin assignment command.
    pub fn new(io: T, address: KatapultCanAddress) -> Self {
        Self { io, address }
    }

    /// Returns the underlying CAN I/O implementation.
    pub fn into_io(self) -> T {
        self.io
    }
}

impl<T: CanIo> KatapultCanTransport<T> {
    /// Requests that the configured Klipper CAN MCU enter Katapult.
    ///
    /// This does not wait for bootloader startup or assign Katapult's temporary
    /// node ID. Call [`Self::assign_node`] only after bootloader entry.
    pub fn request_bootloader_entry(&mut self) -> Result<(), CanTransportError<T::Error>> {
        self.io
            .write(self.address.bootloader_entry_frame())
            .map_err(CanTransportError::Io)
    }

    /// Assigns Katapult's temporary node ID for this explicitly supplied UUID.
    pub fn assign_node(&mut self) -> Result<(), CanTransportError<T::Error>> {
        self.io
            .write(self.address.assignment_frame())
            .map_err(CanTransportError::Io)
    }
}

impl<T: CanIo> Transport for KatapultCanTransport<T> {
    type Error = CanTransportError<T::Error>;

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        for frame in self.address.fragment_request(request) {
            self.io.write(frame).map_err(CanTransportError::Io)?;
        }

        let mut response = Vec::new();
        let mut expected_length = None;
        loop {
            let frame = self.io.read().map_err(CanTransportError::Io)?;
            if frame.id() != self.address.response_id() {
                continue;
            }

            let actual_length = response.len() + frame.data().len();
            if actual_length > MAX_RESPONSE_FRAME_BYTES {
                return Err(CanTransportError::ResponseTooLarge {
                    length: actual_length,
                });
            }
            response.extend_from_slice(frame.data());

            if expected_length.is_none() && response.len() >= 4 {
                expected_length = Some(8 + usize::from(response[3]) * 4);
            }
            if let Some(expected_length) = expected_length {
                if response.len() == expected_length {
                    return Ok(response);
                }
                if response.len() > expected_length {
                    return Err(CanTransportError::ResponseLengthExceeded {
                        expected: expected_length,
                        actual: response.len(),
                    });
                }
            }
        }
    }
}

/// A Linux SocketCAN implementation of [`CanIo`].
///
/// Opening the socket does not assign a Katapult node, enter a bootloader, or
/// transmit a frame. Call [`KatapultCanTransport::assign_node`] explicitly.
pub struct SocketCanIo {
    socket: CanSocket,
}

impl SocketCanIo {
    /// Opens a SocketCAN interface and configures a bounded receive timeout.
    pub fn open(interface: &str, read_timeout: Duration) -> io::Result<Self> {
        let socket = CanSocket::open(interface)?;
        socket.set_read_timeout(read_timeout)?;
        tracing::debug!(
            can_interface = interface,
            read_timeout_ms = read_timeout.as_millis() as u64,
            "opening katapult can session"
        );
        Ok(Self { socket })
    }
}

impl CanIo for SocketCanIo {
    type Error = io::Error;

    fn write(&mut self, frame: CanFrame) -> Result<(), Self::Error> {
        let id = socketcan::StandardId::new(frame.id())
            .expect("CanFrame validates standard CAN identifiers");
        let socket_frame = CanDataFrame::new(id, frame.data())
            .expect("CanFrame validates classic CAN payload length");
        self.socket.write_frame(&socket_frame)
    }

    fn read(&mut self) -> Result<CanFrame, Self::Error> {
        let socketcan::CanFrame::Data(frame) = self.socket.read_frame()? else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Katapult only accepts CAN data frames",
            ));
        };
        let Id::Standard(id) = frame.id() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Katapult only accepts standard CAN frames",
            ));
        };
        CanFrame::new(id.as_raw(), frame.data()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid classic CAN frame received: {error:?}"),
            )
        })
    }
}
