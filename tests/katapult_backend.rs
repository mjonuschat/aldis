use mcu_update::flash::FlashBackend;
use mcu_update::flash::katapult::backend::KatapultBackend;
use mcu_update::flash::katapult::session::Transport;
use mcu_update::flash::katapult::{Command, crc16_ccitt};

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

fn response(command: Command, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x01, 0x88, 0xa0, ((payload.len() + 4) / 4) as u8];
    frame.extend_from_slice(&(command as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc16_ccitt(&frame[2..]).to_le_bytes());
    frame.extend_from_slice(&[0x99, 0x03]);
    frame
}

fn connect_response(application_start: u32, block_size: u32) -> Vec<u8> {
    let protocol_version = [1, 0, 0, 0];
    let application_start = application_start.to_le_bytes();
    let block_size = block_size.to_le_bytes();
    response(
        Command::Connect,
        &[&protocol_version[..], &application_start, &block_size].concat(),
    )
}

#[test]
fn flashes_verifies_and_starts_a_serial_katapult_target() {
    let application_start = 0x0800_8000;
    let mut padded_block = vec![0xab];
    padded_block.resize(64, 0xff);
    let transport = ScriptedTransport {
        responses: vec![
            connect_response(application_start, 64),
            response(Command::SendBlock, &application_start.to_le_bytes()),
            response(Command::SendEof, &1_u32.to_le_bytes()),
            response(
                Command::RequestBlock,
                &[application_start.to_le_bytes().as_slice(), &padded_block].concat(),
            ),
            response(Command::Complete, &[]),
        ],
        ..Default::default()
    };
    let mut backend = KatapultBackend::new(transport);

    assert_eq!(
        backend.flash(&[0xab]).unwrap(),
        mcu_update::flash::FlashResult {
            pages_written: 1,
            padded_bytes: 64,
        }
    );

    let commands: Vec<u8> = backend
        .into_transport()
        .requests
        .iter()
        .map(|request| request[2])
        .collect();
    assert_eq!(
        commands,
        vec![
            Command::Connect as u8,
            Command::SendBlock as u8,
            Command::SendEof as u8,
            Command::RequestBlock as u8,
            Command::Complete as u8,
        ]
    );
}

#[test]
fn verifies_the_configured_can_uuid_before_transferring_firmware() {
    let application_start = 0x0800_2000;
    let mut padded_block = vec![0xcd];
    padded_block.resize(64, 0xff);
    let transport = ScriptedTransport {
        responses: vec![
            connect_response(application_start, 64),
            response(
                Command::GetCanbusId,
                &[0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3, 0, 0],
            ),
            response(Command::SendBlock, &application_start.to_le_bytes()),
            response(Command::SendEof, &1_u32.to_le_bytes()),
            response(
                Command::RequestBlock,
                &[application_start.to_le_bytes().as_slice(), &padded_block].concat(),
            ),
            response(Command::Complete, &[]),
        ],
        ..Default::default()
    };
    let mut backend = KatapultBackend::for_canbus(transport, 0xe781_9ed8_e7d3);

    backend.flash(&[0xcd]).unwrap();

    let commands: Vec<u8> = backend
        .into_transport()
        .requests
        .iter()
        .map(|request| request[2])
        .collect();
    assert_eq!(commands[1], Command::GetCanbusId as u8);
    assert_eq!(commands[2], Command::SendBlock as u8);
}
