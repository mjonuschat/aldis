use mcu_update::flash::katapult::bootstrap::request_can_bootloader;
use mcu_update::flash::katapult::can::{CanFrame, CanIo};

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
    let bootstrap = request_can_bootloader(ScriptedCanIo::default(), 0xe781_9ed8_e7d3).unwrap();

    assert_eq!(
        bootstrap.into_transport().into_io().written,
        vec![CanFrame::new(0x3f0, &[0x02, 0xe7, 0x81, 0x9e, 0xd8, 0xe7, 0xd3]).unwrap(),]
    );

    let backend = request_can_bootloader(ScriptedCanIo::default(), 0xe781_9ed8_e7d3)
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
