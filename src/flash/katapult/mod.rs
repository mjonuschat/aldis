//! Katapult bootloader frame encoding and transport-neutral session logic.

pub mod backend;
pub mod can;
pub mod serial;
pub mod session;

const HEADER: [u8; 2] = [0x01, 0x88];
const TRAILER: [u8; 2] = [0x99, 0x03];
const MAX_PAYLOAD_BYTES: usize = u8::MAX as usize * 4;
const MIN_FRAME_LENGTH: usize = 8;

/// The maximum size of a complete Katapult reply frame.
pub const MAX_RESPONSE_FRAME_BYTES: usize = MIN_FRAME_LENGTH + MAX_PAYLOAD_BYTES;

/// Commands understood by Katapult's bootloader protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Command {
    /// Negotiate protocol and target geometry.
    Connect = 0x11,
    /// Write one padded firmware block.
    SendBlock = 0x12,
    /// Finalize a firmware transfer.
    SendEof = 0x13,
    /// Read one firmware block back.
    RequestBlock = 0x14,
    /// Exit the bootloader and start the application.
    Complete = 0x15,
    /// Return Katapult's CAN UUID.
    GetCanbusId = 0x16,
}

/// Errors that prevent a valid Katapult frame or response from being decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameError {
    /// The payload cannot be expressed as complete four-byte words.
    PayloadNotWordAligned { length: usize },
    /// The one-byte word count cannot represent this payload.
    PayloadTooLarge { length: usize },
    /// A reply is shorter than the smallest Katapult frame.
    ResponseTooShort { length: usize },
    /// A reply exceeds Katapult's representable frame size.
    ResponseTooLarge { length: usize },
    /// The reply does not begin with Katapult's header.
    InvalidResponseHeader,
    /// The reply does not end with Katapult's trailer.
    InvalidResponseTrailer,
    /// The declared payload length and received frame length disagree.
    ResponseLengthMismatch { expected: usize, actual: usize },
    /// The response CRC is invalid.
    ResponseCrcMismatch { expected: u16, actual: u16 },
    /// Katapult rejected the request.
    ResponseRejected { status: u8 },
    /// A successful reply omitted the echoed command word.
    ResponseMissingCommand,
}

/// A successful Katapult response with its echoed command removed from data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// The echoed Katapult command.
    pub command: u32,
    /// Command-specific response bytes.
    pub data: Vec<u8>,
}

/// Calculates Katapult's CRC16-CCITT variant.
pub fn crc16_ccitt(buffer: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for &byte in buffer {
        let mut data = u16::from(byte) ^ (crc & 0xff);
        data ^= (data & 0x0f) << 4;
        crc = ((data << 8) | (crc >> 8)) ^ (data >> 4) ^ (data << 3);
    }
    crc
}

/// Encodes one word-aligned Katapult bootloader command.
pub fn encode_command(command: Command, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if !payload.len().is_multiple_of(4) {
        return Err(FrameError::PayloadNotWordAligned {
            length: payload.len(),
        });
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge {
            length: payload.len(),
        });
    }
    let mut frame = Vec::with_capacity(HEADER.len() + 2 + payload.len() + 4);
    frame.extend_from_slice(&HEADER);
    frame.push(command as u8);
    frame.push((payload.len() / 4) as u8);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc16_ccitt(&frame[2..]).to_le_bytes());
    frame.extend_from_slice(&TRAILER);
    Ok(frame)
}

/// Validates and decodes one complete Katapult response frame.
pub fn decode_response(frame: &[u8]) -> Result<Response, FrameError> {
    const ACK_SUCCESS: u8 = 0xa0;
    if frame.len() < MIN_FRAME_LENGTH {
        return Err(FrameError::ResponseTooShort {
            length: frame.len(),
        });
    }
    if frame.len() > MAX_RESPONSE_FRAME_BYTES {
        return Err(FrameError::ResponseTooLarge {
            length: frame.len(),
        });
    }
    if frame[..2] != HEADER {
        return Err(FrameError::InvalidResponseHeader);
    }
    if frame[frame.len() - 2..] != TRAILER {
        return Err(FrameError::InvalidResponseTrailer);
    }
    let payload_length = usize::from(frame[3]) * 4;
    let expected_length = MIN_FRAME_LENGTH + payload_length;
    if frame.len() != expected_length {
        return Err(FrameError::ResponseLengthMismatch {
            expected: expected_length,
            actual: frame.len(),
        });
    }
    let actual_crc = u16::from_le_bytes([frame[frame.len() - 4], frame[frame.len() - 3]]);
    let expected_crc = crc16_ccitt(&frame[2..frame.len() - 4]);
    if actual_crc != expected_crc {
        return Err(FrameError::ResponseCrcMismatch {
            expected: expected_crc,
            actual: actual_crc,
        });
    }
    if frame[2] != ACK_SUCCESS {
        return Err(FrameError::ResponseRejected { status: frame[2] });
    }
    if payload_length < 4 {
        return Err(FrameError::ResponseMissingCommand);
    }
    let payload = &frame[4..4 + payload_length];
    let command = u32::from_le_bytes(payload[..4].try_into().expect("length was checked"));
    Ok(Response {
        command,
        data: payload[4..].to_vec(),
    })
}
