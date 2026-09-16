use aldis::flash::katapult::session::{KatapultSession, KatapultTarget, SessionError, Transport};
use aldis::flash::katapult::{Command, crc16_ccitt};

#[derive(Default)]
struct ScriptedTransport {
    requests: Vec<Vec<u8>>,
    responses: Vec<Vec<u8>>,
}

impl Transport for ScriptedTransport {
    type Error = ();
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        self.requests.push(request.to_vec());
        Ok(self.responses.remove(0))
    }
}

struct FailingTransport;

impl Transport for FailingTransport {
    type Error = &'static str;
    fn exchange(&mut self, _request: &[u8]) -> Result<Vec<u8>, Self::Error> {
        Err("bus off")
    }
}

fn response(command: Command, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x01, 0x88, 0xa0, ((payload.len() + 4) / 4) as u8];
    frame.extend_from_slice(&(command as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc16_ccitt(&frame[2..]).to_le_bytes());
    frame.extend_from_slice(&[0x99, 0x03]);
    frame
}

#[test]
fn retries_a_malformed_response_before_accepting_the_expected_command() {
    let transport = ScriptedTransport {
        responses: vec![vec![0x00], response(Command::Complete, &[])],
        ..Default::default()
    };
    let mut session = KatapultSession::new(transport);

    session.complete().expect("second response should complete");

    assert_eq!(session.into_transport().requests.len(), 2);
}

#[test]
fn uploads_padded_blocks_then_verifies_each_block() {
    let application_start: u32 = 0x0800_8000;
    let mut padded_block = vec![0xab];
    padded_block.resize(64, 0xff);
    let transport = ScriptedTransport {
        responses: vec![
            response(Command::SendBlock, &application_start.to_le_bytes()),
            response(Command::SendEof, &1_u32.to_le_bytes()),
            response(
                Command::RequestBlock,
                &[application_start.to_le_bytes().as_slice(), &padded_block].concat(),
            ),
        ],
        ..Default::default()
    };
    let mut session = KatapultSession::new(transport);

    let result = session
        .upload(&[0xab], KatapultTarget::new(application_start, 64).unwrap())
        .unwrap();

    assert_eq!(result.pages_written, 1);
    assert_eq!(result.padded_bytes, 64);
}

#[test]
fn verifies_the_explicit_canbus_uuid() {
    let transport = ScriptedTransport {
        responses: vec![response(
            Command::GetCanbusId,
            &[0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3, 0, 0],
        )],
        ..Default::default()
    };
    let mut session = KatapultSession::new(transport);

    session.verify_canbus_uuid(0xe781_9ed8_e7d3).unwrap();
}

#[test]
fn rejects_corrupted_readback() {
    let application_start: u32 = 0x0800_8000;
    let transport = ScriptedTransport {
        responses: vec![
            response(Command::SendBlock, &application_start.to_le_bytes()),
            response(Command::SendEof, &1_u32.to_le_bytes()),
            response(
                Command::RequestBlock,
                &[
                    application_start.to_le_bytes().as_slice(),
                    &[0xac],
                    &[0xff; 63],
                ]
                .concat(),
            ),
        ],
        ..Default::default()
    };
    let mut session = KatapultSession::new(transport);

    assert_eq!(
        session.upload(&[0xab], KatapultTarget::new(application_start, 64).unwrap()),
        Err(SessionError::ChecksumMismatch)
    );
}

#[test]
fn reports_the_underlying_transport_error_once_retries_are_exhausted() {
    let mut session = KatapultSession::new(FailingTransport);

    let error = session.complete().unwrap_err();

    let SessionError::RetriesExhausted { last_failure, .. } = error else {
        panic!("expected RetriesExhausted, got {error:?}");
    };
    assert!(
        last_failure.contains("bus off"),
        "expected the transport's own error in {last_failure:?}"
    );
}
