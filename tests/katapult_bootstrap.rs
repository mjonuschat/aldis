use std::time::Duration;

use aldis::flash::katapult::bootstrap::{confirm_bootloader, request_can_bootloader};
use aldis::flash::katapult::can::{CanFrame, CanIo};
use aldis::flash::katapult::session::{SessionError, Transport};
use aldis::flash::katapult::{Command, crc16_ccitt, encode_command};

#[derive(Default)]
struct ScriptedCanIo {
    written: Vec<CanFrame>,
}

impl CanIo for ScriptedCanIo {
    type Error = ();

    fn write(&mut self, frame: CanFrame) -> Result<(), Self::Error> {
        self.written.push(frame);
        Ok(())
    }

    fn read(&mut self) -> Result<CanFrame, Self::Error> {
        Err(())
    }
}

#[test]
fn separates_can_reboot_from_katapult_node_assignment() {
    let bootstrap = request_can_bootloader(
        ScriptedCanIo::default(),
        0xe781_9ed8_e7d3,
        Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(
        bootstrap.into_transport().into_io().written,
        vec![CanFrame::new(0x3f0, &[0x02, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3]).unwrap(),]
    );

    let backend = request_can_bootloader(
        ScriptedCanIo::default(),
        0xe781_9ed8_e7d3,
        Duration::from_secs(1),
    )
    .unwrap()
    .connect()
    .unwrap();
    assert_eq!(
        backend.into_transport().into_io().written,
        vec![
            CanFrame::new(0x3f0, &[0x02, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3]).unwrap(),
            CanFrame::new(0x3f0, &[0x11, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3, 0x81]).unwrap(),
        ]
    );
}

#[derive(Default)]
struct ScriptedTransport {
    requests: Vec<Vec<u8>>,
    responses: Vec<Vec<u8>>,
}

impl Transport for ScriptedTransport {
    type Error = ();

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        self.requests.push(request.to_vec());
        if self.responses.is_empty() {
            Err(())
        } else {
            Ok(self.responses.remove(0))
        }
    }
}

fn connect_response() -> Vec<u8> {
    let mut payload = vec![1, 0, 0, 0];
    payload.extend_from_slice(&0x0802_0000_u32.to_le_bytes());
    payload.extend_from_slice(&64_u32.to_le_bytes());
    let mut frame = vec![0x01, 0x88, 0xa0, ((payload.len() + 4) / 4) as u8];
    frame.extend_from_slice(&(Command::Connect as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&crc16_ccitt(&frame[2..]).to_le_bytes());
    frame.extend_from_slice(&[0x99, 0x03]);
    frame
}

#[test]
fn confirms_katapult_with_only_the_connect_command() {
    let backend = confirm_bootloader(ScriptedTransport {
        responses: vec![connect_response()],
        ..Default::default()
    })
    .unwrap();

    let requests = backend.into_transport().requests;
    assert_eq!(
        requests,
        vec![encode_command(Command::Connect, &[]).unwrap()]
    );
}

#[test]
fn sends_nothing_but_connect_when_katapult_never_answers() {
    let transport = ScriptedTransport::default();
    let mut seen = Vec::new();
    let error = match confirm_bootloader(RecordingTransport {
        inner: transport,
        seen: &mut seen,
    }) {
        Ok(_) => panic!("an unanswered bootloader must not be confirmed"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        SessionError::RetriesExhausted {
            command: Command::Connect,
            ..
        }
    ));
    assert!(!seen.is_empty());
    let connect = encode_command(Command::Connect, &[]).unwrap();
    assert!(seen.iter().all(|request| *request == connect));
}

struct RecordingTransport<'a> {
    inner: ScriptedTransport,
    seen: &'a mut Vec<Vec<u8>>,
}

impl Transport for RecordingTransport<'_> {
    type Error = ();

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        self.seen.push(request.to_vec());
        self.inner.exchange(request)
    }
}
