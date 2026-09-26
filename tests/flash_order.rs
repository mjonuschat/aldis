use aldis::flash_order::{flash_order, is_usb_can_bridge};
use aldis::moonraker::{Mcu, McuInventory, McuTransport};

fn mcu(name: &str, transport: McuTransport, kconfig: &str) -> Mcu {
    Mcu {
        name: name.to_owned(),
        app: None,
        version: Some("v1".to_owned()),
        mcu: "stm32".to_owned(),
        canbus_frequency_hz: None,
        transport: Some(transport),
        kconfig: kconfig.to_owned(),
    }
}

fn can(uuid: u64) -> McuTransport {
    McuTransport::Can {
        interface: "can0".to_owned(),
        uuid,
    }
}

#[test]
fn flashes_the_bridge_after_every_other_selected_mcu() {
    let inventory = McuInventory {
        mcus: vec![
            mcu("mcu", can(1), "CONFIG_USBCANBUS=y\n"),
            mcu("mcu ebb", can(2), "CONFIG_CANBUS=y\n"),
            mcu(
                "mcu host",
                McuTransport::Serial {
                    device: "/tmp/klipper_host_mcu".to_owned(),
                },
                "",
            ),
        ],
        unreported: Vec::new(),
    };
    let targets = vec![
        "mcu".to_owned(),
        "mcu ebb".to_owned(),
        "mcu host".to_owned(),
    ];

    assert_eq!(
        flash_order(&inventory, &targets),
        vec!["mcu ebb", "mcu host", "mcu"]
    );
    assert!(is_usb_can_bridge(&inventory.mcus[0]));
    assert!(!is_usb_can_bridge(&inventory.mcus[1]));
}
