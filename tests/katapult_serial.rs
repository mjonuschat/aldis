use std::collections::VecDeque;
use std::time::{Duration, Instant};

use aldis::flash::katapult::crc16_ccitt;
use aldis::flash::katapult::serial::{
    BOOTLOADER_ENTRY_REQUEST, KatapultSerialTransport, SerialIo, SerialTransportError,
};
use aldis::flash::katapult::session::Transport;

#[derive(Default)]
struct ScriptedSerial {
    writes: Vec<Vec<u8>>,
    reads: VecDeque<Vec<u8>>,
}

impl SerialIo for ScriptedSerial {
    type Error = ();

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn read(&mut self) -> Result<Vec<u8>, Self::Error> {
        self.reads.pop_front().ok_or(())
    }
}

#[test]
fn reassembles_a_fragmented_katapult_reply() {
    let mut reply = vec![1, 0x88, 0xa0, 1, 0x15, 0, 0, 0];
    reply.extend_from_slice(&crc16_ccitt(&reply[2..]).to_le_bytes());
    reply.extend_from_slice(&[0x99, 3]);
    let io = ScriptedSerial {
        reads: VecDeque::from([reply[..3].to_vec(), reply[3..].to_vec()]),
        ..Default::default()
    };
    let mut transport = KatapultSerialTransport::new(io);

    assert_eq!(transport.exchange(&[1, 2]).unwrap(), reply);
    assert_eq!(transport.into_io().writes, vec![vec![1, 2]]);
    assert_eq!(
        BOOTLOADER_ENTRY_REQUEST,
        b"~ \x1c Request Serial Bootloader!! ~"
    );
}

fn complete_reply() -> Vec<u8> {
    let mut reply = vec![1, 0x88, 0xa0, 1, 0x15, 0, 0, 0];
    reply.extend_from_slice(&crc16_ccitt(&reply[2..]).to_le_bytes());
    reply.extend_from_slice(&[0x99, 3]);
    reply
}

#[test]
fn discards_the_tail_of_a_late_reply_ahead_of_the_next_frames_header() {
    let reply = complete_reply();
    // The last bytes of a reply to an earlier, timed-out request arrive first, then the real
    // reply split so the header itself straddles two chunks.
    let mut first = vec![0, 0, 0x99, 0x03, 0x01];
    first.extend_from_slice(&reply[1..5]);
    let io = ScriptedSerial {
        reads: VecDeque::from([first, reply[5..].to_vec()]),
        ..Default::default()
    };
    let mut transport = KatapultSerialTransport::new(io);

    assert_eq!(transport.exchange(&[1, 2]).unwrap(), reply);
}

/// Never blocks and never produces a frame header.
struct NoiseSerial;

impl SerialIo for NoiseSerial {
    type Error = ();

    fn write_all(&mut self, _: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn read(&mut self) -> Result<Vec<u8>, Self::Error> {
        Ok(vec![0x55; 16])
    }
}

#[test]
fn times_out_an_exchange_fed_bytes_that_never_form_a_frame() {
    let started = Instant::now();
    let mut transport =
        KatapultSerialTransport::with_timeout(NoiseSerial, Duration::from_millis(50));

    assert!(matches!(
        transport.exchange(&[1, 2]),
        Err(SerialTransportError::Timeout)
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn reports_a_serial_device_that_returns_no_bytes_instead_of_spinning() {
    let io = ScriptedSerial {
        reads: VecDeque::from([Vec::new()]),
        ..Default::default()
    };
    let mut transport = KatapultSerialTransport::new(io);

    assert!(matches!(
        transport.exchange(&[1, 2]),
        Err(SerialTransportError::Disconnected)
    ));
}

#[test]
fn keeps_a_trailing_header_byte_that_the_next_chunk_completes() {
    let reply = complete_reply();
    let io = ScriptedSerial {
        reads: VecDeque::from([vec![0x7f, 0x01], reply[1..].to_vec()]),
        ..Default::default()
    };
    let mut transport = KatapultSerialTransport::new(io);

    assert_eq!(transport.exchange(&[1, 2]).unwrap(), reply);
}
