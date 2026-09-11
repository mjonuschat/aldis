use mcu_update::flash::stm32_dfu::target_from_kconfig;

#[test]
fn derives_stm32_application_start_from_embedded_kconfig() {
    assert_eq!(
        target_from_kconfig("CONFIG_STM32_FLASH_START_2000=y\n", 2048)
            .unwrap()
            .application_start,
        0x0800_2000
    );
}
