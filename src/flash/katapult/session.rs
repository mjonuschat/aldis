//! Retry and response-validation logic shared by Katapult transports.

use std::fmt;

use sha1::{Digest, Sha1};

use super::{Command, FrameError, decode_response, encode_command};

const MAX_ATTEMPTS: usize = 5;
const BLOCK_ATTEMPTS: usize = 3;

/// Exchanges complete Katapult frames over one sequential transport.
pub trait Transport {
    /// Transport-specific error type.
    type Error;

    /// Sends one complete request and returns one complete response.
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error>;
}

/// Errors returned by a Katapult session.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SessionError {
    /// The request frame could not be encoded.
    #[error("could not encode request frame: {0}")]
    Encode(#[source] FrameError),
    /// Katapult advertised unsupported target geometry.
    #[error("unsupported target geometry: {0}")]
    Target(#[source] TargetError),
    /// The bounded retry policy could not obtain a valid response.
    #[error("no valid response to {command:?} after retrying: {last_failure}")]
    RetriesExhausted {
        command: Command,
        last_failure: String,
    },
    /// A response did not contain required data.
    #[error("{command:?} response too short: expected at least {expected} bytes, got {actual}")]
    ResponseDataTooShort {
        command: Command,
        expected: usize,
        actual: usize,
    },
    /// A response did not have the exact required data length.
    #[error("{command:?} response length mismatch: expected {expected} bytes, got {actual}")]
    ResponseDataLengthMismatch {
        command: Command,
        expected: usize,
        actual: usize,
    },
    /// Katapult acknowledged a different address than requested.
    #[error("{command:?} acknowledged address {actual:#x}, expected {expected:#x}")]
    AddressMismatch {
        command: Command,
        expected: u32,
        actual: u32,
    },
    /// Firmware block addressing exceeded a 32-bit application address.
    #[error("firmware block address exceeds a 32-bit application address")]
    AddressOverflow,
    /// Readback SHA-1 does not match the transmitted padded data.
    #[error("readback SHA-1 does not match the transmitted padded data")]
    ChecksumMismatch,
    /// Katapult's CAN UUID does not match Moonraker's configured UUID.
    #[error("Katapult CAN UUID {actual:#x} does not match configured UUID {expected:#x}")]
    CanbusUuidMismatch { expected: u64, actual: u64 },
}

/// Katapult firmware geometry reported by `CONNECT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KatapultTarget {
    application_start: u32,
    block_size: usize,
}

/// An unsupported Katapult target geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TargetError {
    /// Katapult advertised a block size not supported by the protocol implementation.
    #[error("unsupported Katapult block size: {block_size}")]
    UnsupportedBlockSize { block_size: u32 },
}

impl KatapultTarget {
    /// Validates supported geometry supplied by Katapult.
    pub fn new(application_start: u32, block_size: u32) -> Result<Self, TargetError> {
        let block_size = match block_size {
            64 | 128 | 256 | 512 => block_size as usize,
            _ => return Err(TargetError::UnsupportedBlockSize { block_size }),
        };
        Ok(Self {
            application_start,
            block_size,
        })
    }
}

/// Details of a Katapult upload and readback verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadResult {
    /// Pages Katapult reports it wrote after EOF.
    pub pages_written: u32,
    /// Transferred bytes including `0xff` padding.
    pub padded_bytes: usize,
}

/// Katapult's negotiated protocol version and target geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KatapultConnection {
    /// Four-byte protocol version.
    pub protocol_version: [u8; 4],
    /// Validated application target geometry.
    pub target: KatapultTarget,
}

/// A Katapult session over an arbitrary sequential transport.
pub struct KatapultSession<T> {
    transport: T,
}

impl<T> KatapultSession<T> {
    /// Creates a session around `transport`.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
    /// Returns ownership of the underlying transport.
    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T: Transport> KatapultSession<T>
where
    T::Error: fmt::Debug,
{
    /// Connects and validates the advertised target geometry.
    pub fn connect(&mut self) -> Result<KatapultConnection, SessionError> {
        let response = self.command(Command::Connect, &[])?;
        if response.len() < 12 {
            return Err(SessionError::ResponseDataTooShort {
                command: Command::Connect,
                expected: 12,
                actual: response.len(),
            });
        }
        let protocol_version = response[..4]
            .try_into()
            .expect("response length was checked");
        let application_start = u32::from_le_bytes(
            response[4..8]
                .try_into()
                .expect("response length was checked"),
        );
        let block_size = u32::from_le_bytes(
            response[8..12]
                .try_into()
                .expect("response length was checked"),
        );
        let target =
            KatapultTarget::new(application_start, block_size).map_err(SessionError::Target)?;
        Ok(KatapultConnection {
            protocol_version,
            target,
        })
    }

    /// Verifies Katapult's reported CAN UUID.
    pub fn verify_canbus_uuid(&mut self, expected: u64) -> Result<(), SessionError> {
        let response = self.command(Command::GetCanbusId, &[])?;
        let bytes = response
            .get(..6)
            .ok_or(SessionError::ResponseDataTooShort {
                command: Command::GetCanbusId,
                expected: 6,
                actual: response.len(),
            })?;
        let actual = bytes
            .iter()
            .fold(0_u64, |uuid, byte| (uuid << 8) | u64::from(*byte));
        if actual == expected {
            Ok(())
        } else {
            Err(SessionError::CanbusUuidMismatch { expected, actual })
        }
    }

    /// Exits Katapult and starts its application.
    pub fn complete(&mut self) -> Result<(), SessionError> {
        self.command(Command::Complete, &[]).map(|_| ())
    }

    /// Sends one command after bounded response validation retries.
    pub fn command(&mut self, command: Command, payload: &[u8]) -> Result<Vec<u8>, SessionError> {
        let request = encode_command(command, payload).map_err(SessionError::Encode)?;
        let mut last_failure = String::from("no response received");
        for _ in 0..MAX_ATTEMPTS {
            let response = match self.transport.exchange(&request) {
                Ok(response) => response,
                Err(error) => {
                    last_failure = format!("transport error: {error:?}");
                    continue;
                }
            };
            let response = match decode_response(&response) {
                Ok(response) => response,
                Err(error) => {
                    last_failure = format!("invalid response: {error:?}");
                    continue;
                }
            };
            if response.command == command as u32 {
                return Ok(response.data);
            }
            last_failure = format!("expected command {command:?}, got 0x{:x}", response.command);
        }
        Err(SessionError::RetriesExhausted {
            command,
            last_failure,
        })
    }

    /// Uploads padded firmware blocks, reads them back, and verifies protocol-required SHA-1.
    pub fn upload(
        &mut self,
        firmware: &[u8],
        target: KatapultTarget,
    ) -> Result<UploadResult, SessionError> {
        let mut written_hash = Sha1::new();
        let block_count = firmware.len().div_ceil(target.block_size);
        for (index, chunk) in firmware.chunks(target.block_size).enumerate() {
            let address = block_address(target, index)?;
            let block = padded_block(chunk, target.block_size);
            written_hash.update(&block);
            self.write_block(address, &block)?;
        }
        let pages_written = response_u32(Command::SendEof, &self.command(Command::SendEof, &[])?)?;
        let mut verified_hash = Sha1::new();
        for index in 0..block_count {
            let address = block_address(target, index)?;
            verified_hash.update(self.read_block(address, target.block_size)?);
        }
        let expected: [u8; 20] = written_hash.finalize().into();
        let actual: [u8; 20] = verified_hash.finalize().into();
        if expected != actual {
            return Err(SessionError::ChecksumMismatch);
        }
        Ok(UploadResult {
            pages_written,
            padded_bytes: block_count * target.block_size,
        })
    }

    fn write_block(&mut self, address: u32, block: &[u8]) -> Result<(), SessionError> {
        let mut payload = address.to_le_bytes().to_vec();
        payload.extend_from_slice(block);
        let mut actual = 0;
        for _ in 0..BLOCK_ATTEMPTS {
            actual = response_u32(
                Command::SendBlock,
                &self.command(Command::SendBlock, &payload)?,
            )?;
            if actual == address {
                return Ok(());
            }
        }
        Err(SessionError::AddressMismatch {
            command: Command::SendBlock,
            expected: address,
            actual,
        })
    }

    fn read_block(&mut self, address: u32, block_size: usize) -> Result<Vec<u8>, SessionError> {
        let mut actual = 0;
        for _ in 0..BLOCK_ATTEMPTS {
            let response = self.command(Command::RequestBlock, &address.to_le_bytes())?;
            if response.len() != 4 + block_size {
                return Err(SessionError::ResponseDataLengthMismatch {
                    command: Command::RequestBlock,
                    expected: 4 + block_size,
                    actual: response.len(),
                });
            }
            actual = response_u32(Command::RequestBlock, &response)?;
            if actual == address {
                return Ok(response[4..].to_vec());
            }
        }
        Err(SessionError::AddressMismatch {
            command: Command::RequestBlock,
            expected: address,
            actual,
        })
    }
}

fn block_address(target: KatapultTarget, index: usize) -> Result<u32, SessionError> {
    let offset = index
        .checked_mul(target.block_size)
        .and_then(|offset| u32::try_from(offset).ok())
        .ok_or(SessionError::AddressOverflow)?;
    target
        .application_start
        .checked_add(offset)
        .ok_or(SessionError::AddressOverflow)
}

fn padded_block(chunk: &[u8], block_size: usize) -> Vec<u8> {
    let mut block = vec![0xff; block_size];
    block[..chunk.len()].copy_from_slice(chunk);
    block
}

fn response_u32(command: Command, response: &[u8]) -> Result<u32, SessionError> {
    let bytes: [u8; 4] = response
        .get(..4)
        .ok_or(SessionError::ResponseDataTooShort {
            command,
            expected: 4,
            actual: response.len(),
        })?
        .try_into()
        .expect("slice length was checked");
    Ok(u32::from_le_bytes(bytes))
}
