use aldis::flash_order::kconfig_selects_usb_can_bridge;
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

#[test]
fn recognizes_the_bridge_symbol_each_architecture_puts_in_its_embedded_kconfig() {
    for kconfig in [
        "CONFIG_MACH_RPXXXX=y\nCONFIG_RPXXXX_USBCANBUS=y\n",
        "CONFIG_MACH_STM32=y\nCONFIG_STM32_USBCANBUS_PA11_PA12=y\n",
        "CONFIG_MACH_ATSAMD=y\nCONFIG_ATSAMD_USBCANBUS=y\n",
        "CONFIG_USBCANBUS=y\n",
    ] {
        assert!(kconfig_selects_usb_can_bridge(kconfig), "{kconfig}");
    }
}

#[test]
fn does_not_mistake_other_configurations_for_a_bridge() {
    for kconfig in [
        "# CONFIG_USBCANBUS is not set\n",
        "CONFIG_HAVE_STM32_USBCANBUS=y\n",
        "CONFIG_MACH_STM32=y\nCONFIG_STM32_USB_PA11_PA12=y\n",
        "CONFIG_RPXXXX_USBCANBUS=n\n",
    ] {
        assert!(!kconfig_selects_usb_can_bridge(kconfig), "{kconfig}");
    }
}
