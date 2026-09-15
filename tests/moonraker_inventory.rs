use aldis::moonraker::{McuTransport, parse_inventory};

#[test]
fn parses_mcu_inventory_from_a_moonraker_object_query() {
    let response = include_str!("fixtures/mcu-inventory.json");

    let inventory = parse_inventory(response).expect("fixture should parse");

    assert_eq!(inventory.mcus.len(), 2);
    assert_eq!(inventory.mcus[0].name, "mcu");
    assert_eq!(inventory.mcus[0].mcu, "stm32f429xx");
    assert_eq!(inventory.mcus[1].name, "mcu toolhead");
    assert_eq!(inventory.mcus[1].mcu, "stm32g0b1xx");
    assert_eq!(inventory.mcus[1].canbus_frequency_hz, Some(1_000_000));
    assert_eq!(
        inventory.mcus[0].transport,
        Some(McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Klipper_stm32f429xx_320050000F50304738313820-if00"
                .to_owned(),
        })
    );
    assert_eq!(
        inventory.mcus[1].transport,
        Some(McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        })
    );
    assert_eq!(
        inventory.mcus[1].kconfig,
        "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_STM32=y\nCONFIG_MACH_STM32G0B1=y\nCONFIG_STM32_MMENU_CANBUS_PB0_PB1=y\n"
    );
}
