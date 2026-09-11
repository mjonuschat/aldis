use mcu_update::flash::stm32_dfu::Stm32DfuError;
use mcu_update::flash::stm32_dfu::target_from_kconfig;

#[test]
fn derives_stm32_application_start_from_embedded_kconfig() {
    assert_eq!(
        target_from_kconfig("CONFIG_STM32_FLASH_START_2000=y")
            .unwrap()
            .application_start,
        0x0800_2000
    );
}

#[test]
fn rejects_missing_or_conflicting_flash_start_symbols() {
    assert!(matches!(
        target_from_kconfig(""),
        Err(Stm32DfuError::InvalidFlashStartConfiguration)
    ));
    let conflicting_config = "CONFIG_STM32_FLASH_START_2000=y\nCONFIG_STM32_FLASH_START_4000=y\n";
    assert!(matches!(
        target_from_kconfig(conflicting_config),
        Err(Stm32DfuError::InvalidFlashStartConfiguration)
    ));
}
