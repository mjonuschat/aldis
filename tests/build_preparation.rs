use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::moonraker::{McuTransport, parse_inventory};
use aldis::prepare::{PreparationError, prepare_build};

#[test]
fn prepares_a_discovered_mcu_without_writing_files() {
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");
    let root = unique_temporary_path();
    let config_path = root.join("toolhead/.config");
    let artifact_path = root.join("artifacts/toolhead.bin");

    let prepared = prepare_build(
        &inventory,
        "mcu toolhead",
        config_path.clone(),
        artifact_path.clone(),
        false,
    )
    .expect("discovered toolhead should prepare");

    assert_eq!(prepared.target_name, "mcu toolhead");
    assert_eq!(prepared.mcu, "stm32g0b1xx");
    assert_eq!(
        prepared.transport,
        Some(McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        })
    );
    assert_eq!(prepared.request.config_path, config_path);
    assert_eq!(prepared.request.artifact_path, artifact_path);
    assert_eq!(
        prepared.request.kconfig,
        "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_STM32=y\nCONFIG_MACH_STM32G0B1=y\nCONFIG_STM32_MMENU_CANBUS_PB0_PB1=y\n"
    );
    assert!(!root.exists());
}

#[test]
fn rejects_a_target_absent_from_the_discovered_inventory() {
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");

    assert_eq!(
        prepare_build(
            &inventory,
            "mcu missing",
            PathBuf::from("missing/.config"),
            PathBuf::from("artifacts/missing.bin"),
            false,
        ),
        Err(PreparationError::TargetNotDiscovered(
            "mcu missing".to_owned()
        ))
    );
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-preparation-{}-{nonce}", std::process::id()))
}
