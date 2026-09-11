use std::collections::VecDeque;

use mcu_update::flash::katapult::crc16_ccitt;
use mcu_update::flash::katapult::serial::{
    BOOTLOADER_ENTRY_REQUEST, KatapultSerialTransport, SerialIo,
};
use mcu_update::flash::katapult::session::Transport;

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
