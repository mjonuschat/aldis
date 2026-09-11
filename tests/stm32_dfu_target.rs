use mcu_update::flash::stm32_dfu::Stm32DfuError;
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

#[test]
fn rejects_missing_or_conflicting_flash_start_symbols() {
    assert!(matches!(
        target_from_kconfig("", 2048),
        Err(Stm32DfuError::InvalidFlashStartConfiguration)
    ));
    assert!(matches!(
        target_from_kconfig(
            "CONFIG_STM32_FLASH_START_2000=y\nCONFIG_STM32_FLASH_START_4000=y\n",
            2048
        ),
        Err(Stm32DfuError::InvalidFlashStartConfiguration)
    ));
}
