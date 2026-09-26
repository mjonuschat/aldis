//! Klipper's `identify` exchange, the only part of the MCU protocol aldis speaks directly.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::moonraker::{KlippyState, Mcu, McuInventory, McuTransport, UnreportedReason};

const MESSAGE_MIN: usize = 5;
const MESSAGE_MAX: usize = 64;
const MESSAGE_DEST: u8 = 0x10;
const MESSAGE_SEQ_MASK: u8 = 0x0f;
const MESSAGE_SYNC: u8 = 0x7e;
const IDENTIFY_RESPONSE_ID: u32 = 0;
const IDENTIFY_ID: u32 = 1;
const IDENTIFY_CHUNK: u8 = 40;

/// Failure while identifying an MCU directly.
#[derive(Debug, thiserror::Error)]
pub enum IdentifyError {
    /// The port could not be opened (typically because Klippy holds it exclusively).
    #[error("could not open the MCU port")]
    PortUnavailable(#[source] io::Error),
    /// The MCU never answered.
    #[error("the MCU did not answer identify requests")]
    NoResponse,
    /// Reading or writing the port failed.
    #[error("MCU port I/O failed")]
    Io(#[source] io::Error),
    /// The data dictionary could not be decoded.
    #[error("could not decode the MCU data dictionary: {0}")]
    Decode(String),
}

/// Klipper's CRC-16/CCITT variant (`msgproto.crc16_ccitt`).
pub fn crc16_ccitt(buf: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in buf {
        let mut data = byte ^ (crc & 0xff) as u8;
        data ^= (data & 0x0f) << 4;
        let data = u16::from(data);
        crc = ((data << 8) | (crc >> 8)) ^ (data >> 4) ^ (data << 3);
    }
    crc
}

/// Klipper's variable-length integer encoding (`PT_uint32.encode`), unsigned values only.
pub fn encode_vlq(out: &mut Vec<u8>, value: u32) {
    if value >= 0xc00_0000 {
        out.push(((value >> 28) & 0x7f) as u8 | 0x80);
    }
    if value >= 0x18_0000 {
        out.push(((value >> 21) & 0x7f) as u8 | 0x80);
    }
    if value >= 0x3000 {
        out.push(((value >> 14) & 0x7f) as u8 | 0x80);
    }
    if value >= 0x60 {
        out.push(((value >> 7) & 0x7f) as u8 | 0x80);
    }
    out.push((value & 0x7f) as u8);
}

/// Decodes one unsigned VLQ integer at `pos`, advancing it.
pub fn decode_vlq(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut byte = *buf.get(*pos)?;
    *pos += 1;
    let mut value = u32::from(byte & 0x7f);
    if byte & 0x60 == 0x60 {
        value |= !0x1f;
    }
    while byte & 0x80 != 0 {
        byte = *buf.get(*pos)?;
        *pos += 1;
        value = (value << 7) | u32::from(byte & 0x7f);
    }
    Some(value)
}

/// Wraps `payload` in a Klipper message block with sequence number `seq`.
pub fn encode_frame(seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + MESSAGE_MIN);
    frame.push((payload.len() + MESSAGE_MIN) as u8);
    frame.push(MESSAGE_DEST | (seq & MESSAGE_SEQ_MASK));
    frame.extend_from_slice(payload);
    let crc = crc16_ccitt(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame.push(MESSAGE_SYNC);
    frame
}

/// The `identify offset=… count=…` command frame.
pub fn identify_request(seq: u8, offset: u32, count: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    encode_vlq(&mut payload, IDENTIFY_ID);
    encode_vlq(&mut payload, offset);
    encode_vlq(&mut payload, u32::from(count));
    encode_frame(seq, &payload)
}

/// One validated message block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The raw sequence byte (`0x10 | seq`).
    pub seq: u8,
    /// The message payload; empty for an ACK/NAK.
    pub payload: Vec<u8>,
}

/// Reassembles frames from a byte stream, discarding bytes until a valid block starts.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let &len = self.buffer.first()?;
            let len = usize::from(len);
            if !(MESSAGE_MIN..=MESSAGE_MAX).contains(&len) {
                self.buffer.remove(0);
                continue;
            }
            if self.buffer.len() < len {
                return None;
            }
            let block = &self.buffer[..len];
            let crc = u16::from_be_bytes([block[len - 3], block[len - 2]]);
            let valid = block[len - 1] == MESSAGE_SYNC
                && block[1] & !MESSAGE_SEQ_MASK == MESSAGE_DEST
                && crc16_ccitt(&block[..len - 3]) == crc;
            if !valid {
                self.buffer.remove(0);
                continue;
            }
            let frame = Frame {
                seq: block[1],
                payload: block[2..len - 3].to_vec(),
            };
            self.buffer.drain(..len);
            return Some(frame);
        }
    }
}

/// Parses an `identify_response` payload into its offset and data chunk.
pub fn parse_identify_response(payload: &[u8]) -> Option<(u32, Vec<u8>)> {
    let mut pos = 0;
    if decode_vlq(payload, &mut pos)? != IDENTIFY_RESPONSE_ID {
        return None;
    }
    let offset = decode_vlq(payload, &mut pos)?;
    let len = usize::from(*payload.get(pos)?);
    let data = payload.get(pos + 1..pos + 1 + len)?;
    Some((offset, data.to_vec()))
}

/// The fields aldis needs from an MCU's data dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifyData {
    pub app: Option<String>,
    pub version: String,
    pub mcu: String,
    pub canbus_frequency_hz: Option<u64>,
    pub kconfig: String,
}

impl IdentifyData {
    /// Decodes the zlib-compressed JSON dictionary returned by `identify`.
    pub fn from_compressed(data: &[u8]) -> Result<Self, IdentifyError> {
        let mut json = String::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_string(&mut json)
            .map_err(|error| IdentifyError::Decode(error.to_string()))?;
        let json: Value = serde_json::from_str(&json)
            .map_err(|error| IdentifyError::Decode(error.to_string()))?;
        let text = |pointer: &str| {
            json.pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        Ok(Self {
            app: text("/app"),
            version: text("/version")
                .ok_or_else(|| IdentifyError::Decode("no version".to_owned()))?,
            mcu: text("/config/MCU").ok_or_else(|| IdentifyError::Decode("no MCU".to_owned()))?,
            canbus_frequency_hz: json
                .pointer("/config/CANBUS_FREQUENCY")
                .and_then(Value::as_u64),
            kconfig: text("/kconfig").unwrap_or_default(),
        })
    }
}

/// Runs the identify exchange over an open port and returns the compressed dictionary.
///
/// Starts at sequence 0 and adopts whatever sequence the MCU reports in each reply (including a
/// NAK), since firmware that talked to Klippy earlier expects a sequence number other than 0.
pub fn identify_over<P: Read + Write>(
    port: &mut P,
    timeout: Duration,
) -> Result<Vec<u8>, IdentifyError> {
    let deadline = Instant::now() + timeout;
    port.write_all(&[MESSAGE_SYNC]).map_err(IdentifyError::Io)?;
    let mut decoder = FrameDecoder::new();
    let mut seq = 0u8;
    let mut data = Vec::new();
    let mut buffer = [0u8; MESSAGE_MAX];
    loop {
        port.write_all(&identify_request(seq, data.len() as u32, IDENTIFY_CHUNK))
            .map_err(IdentifyError::Io)?;
        let frame = loop {
            if let Some(frame) = decoder.next_frame() {
                break frame;
            }
            if Instant::now() >= deadline {
                return Err(IdentifyError::NoResponse);
            }
            match port.read(&mut buffer) {
                Ok(n) => decoder.push(&buffer[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(IdentifyError::Io(error)),
            }
        };
        seq = frame.seq & MESSAGE_SEQ_MASK;
        let Some((offset, chunk)) = parse_identify_response(&frame.payload) else {
            continue;
        };
        if offset as usize != data.len() {
            continue;
        }
        if chunk.is_empty() {
            return Ok(data);
        }
        data.extend_from_slice(&chunk);
    }
}

/// Something that can identify the Klipper firmware behind a serial device.
pub trait IdentifyPort {
    fn identify(&self, device: &str) -> Result<IdentifyData, IdentifyError>;
}

/// Identifies over a real serial port, opened exclusively.
#[derive(Debug, Clone)]
pub struct SerialIdentify {
    pub timeout: Duration,
}

impl Default for SerialIdentify {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }
}

impl IdentifyPort for SerialIdentify {
    fn identify(&self, device: &str) -> Result<IdentifyData, IdentifyError> {
        if !std::path::Path::new(device).exists() {
            return Err(IdentifyError::NoResponse);
        }
        // serialport opens TTYs with TIOCEXCL, so a port Klippy still holds fails here instead of
        // letting two hosts talk to one MCU. The baud rate is ignored by USB CDC; 250000 is
        // Klipper's UART default.
        let mut port = serialport::new(device, 250_000)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|error| IdentifyError::PortUnavailable(error.into()))?;
        let compressed = identify_over(&mut port, self.timeout)?;
        IdentifyData::from_compressed(&compressed)
    }
}

/// Whether discovery in `state` may probe unreported MCUs. Every update stops Klipper before
/// touching hardware, so in `error`/`shutdown` no flash can be in progress.
pub fn should_probe(state: &KlippyState) -> bool {
    matches!(state, KlippyState::Error | KlippyState::Shutdown)
}

/// Identifies unreported serial MCUs directly, moving each one that answers into `mcus`.
/// CAN and host MCUs are never probed and stay `NotIdentified`.
pub fn resolve_unreported(inventory: &mut McuInventory, prober: &impl IdentifyPort) {
    let mut remaining = Vec::new();
    for mut unreported in std::mem::take(&mut inventory.unreported) {
        let Some(McuTransport::Serial { device }) = &unreported.transport else {
            remaining.push(unreported);
            continue;
        };
        match prober.identify(device) {
            Ok(data) => inventory.mcus.push(Mcu {
                name: unreported.name,
                app: data.app,
                version: Some(data.version),
                mcu: data.mcu,
                canbus_frequency_hz: data.canbus_frequency_hz,
                transport: unreported.transport,
                kconfig: data.kconfig,
            }),
            Err(IdentifyError::PortUnavailable(_)) => {
                unreported.reason = UnreportedReason::NotIdentified;
                remaining.push(unreported);
            }
            Err(error) => {
                tracing::debug!(name = %unreported.name, ?error, "direct identify failed");
                unreported.reason = UnreportedReason::NotResponding;
                remaining.push(unreported);
            }
        }
    }
    remaining.sort_by(|a, b| a.name.cmp(&b.name));
    inventory.unreported = remaining;
    inventory.mcus.sort_by(|a, b| a.name.cmp(&b.name));
}
