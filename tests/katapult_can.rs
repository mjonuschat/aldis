use std::collections::VecDeque;
use std::time::Duration;

use aldis::flash::katapult::can::{
    CanFrame, CanIo, CanTransportError, KatapultCanAddress, KatapultCanTransport,
};
use aldis::flash::katapult::session::Transport;

#[test]
fn times_out_an_exchange_flooded_with_unrelated_can_traffic() {
    let address = KatapultCanAddress::new(0xe781_9ed8_e7d3).unwrap();
    let io = UnrelatedTrafficCanIo { other_id: 0x3f1 };
    let mut transport = KatapultCanTransport::new(io, address, Duration::from_millis(50));

    assert!(matches!(
        transport.exchange(&[0x01]),
        Err(CanTransportError::Timeout)
    ));
}

struct UnrelatedTrafficCanIo {
    other_id: u16,
}

impl CanIo for UnrelatedTrafficCanIo {
    type Error = ();

    fn write(&mut self, _frame: CanFrame) -> Result<(), Self::Error> {
        Ok(())
    }

    fn read(&mut self) -> Result<CanFrame, Self::Error> {
        Ok(CanFrame::new(self.other_id, &[0; 8]).unwrap())
    }
}

#[test]
fn assigns_the_explicit_uuid_and_fragments_protocol_frames() {
    let address = KatapultCanAddress::new(0xe781_9ed8_e7d3).unwrap();

    assert_eq!(
        address.assignment_frame(),
        CanFrame::new(0x3f0, &[0x11, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3, 0x81]).unwrap()
    );
    assert_eq!(address.request_id(), 0x202);
    assert_eq!(address.response_id(), 0x203);
    assert_eq!(
        address.bootloader_entry_frame(),
        CanFrame::new(0x3f0, &[0x02, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3]).unwrap()
    );
    assert_eq!(
        address.fragment_request(&[0; 10]),
        vec![
            CanFrame::new(0x202, &[0; 8]).unwrap(),
            CanFrame::new(0x202, &[0; 2]).unwrap(),
        ]
    );
}

#[test]
fn rejects_response_assembly_larger_than_katapult_can_represent() {
    let address = KatapultCanAddress::new(0xe781_9ed8_e7d3).unwrap();
    let mut received =
        VecDeque::from([
            CanFrame::new(address.response_id(), &[1, 0x88, 0xa0, 255, 0, 0, 0, 0]).unwrap(),
        ]);
    received.extend((0..128).map(|_| CanFrame::new(address.response_id(), &[0; 8]).unwrap()));
    let io = ScriptedCanIo {
        received,
        ..Default::default()
    };
    let mut transport = KatapultCanTransport::new(io, address, Duration::from_secs(1));

    assert!(matches!(
        transport.exchange(&[]),
        Err(CanTransportError::ResponseTooLarge { length: 1032 })
    ));
}

#[derive(Default)]
struct ScriptedCanIo {
    written: Vec<CanFrame>,
    received: VecDeque<CanFrame>,
}

impl CanIo for ScriptedCanIo {
    type Error = ();

    fn write(&mut self, frame: CanFrame) -> Result<(), Self::Error> {
        self.written.push(frame);
        Ok(())
    }

    fn read(&mut self) -> Result<CanFrame, Self::Error> {
        self.received.pop_front().ok_or(())
    }
}

#[test]
fn assigns_a_node_then_reassembles_only_its_response_frames() {
    let address = KatapultCanAddress::new(0xe781_9ed8_e7d3).unwrap();
    let response = [0x01, 0x88, 0xa0, 0x01, 0x15, 0, 0, 0, 0, 0, 0x99, 0x03];
    let io = ScriptedCanIo {
        received: VecDeque::from([
            CanFrame::new(0x3f1, &[0; 8]).unwrap(),
            CanFrame::new(address.response_id(), &response[..8]).unwrap(),
            CanFrame::new(address.response_id(), &response[8..]).unwrap(),
        ]),
        ..Default::default()
    };
    let mut transport = KatapultCanTransport::new(io, address, Duration::from_secs(1));

    transport.assign_node().unwrap();
    assert_eq!(
        transport.exchange(&[0x01, 0x88, 0x15, 0]).unwrap(),
        response
    );

    let io = transport.into_io();
    assert_eq!(io.written[0], address.assignment_frame());
    assert_eq!(
        io.written[1],
        CanFrame::new(address.request_id(), &[0x01, 0x88, 0x15, 0]).unwrap()
    );
}

#[test]
fn requests_bootloader_entry_without_assigning_a_katapult_node() {
    let address = KatapultCanAddress::new(0xe781_9ed8_e7d3).unwrap();
    let mut transport =
        KatapultCanTransport::new(ScriptedCanIo::default(), address, Duration::from_secs(1));

    transport.request_bootloader_entry().unwrap();

    assert_eq!(
        transport.into_io().written,
        vec![address.bootloader_entry_frame()]
    );
}
