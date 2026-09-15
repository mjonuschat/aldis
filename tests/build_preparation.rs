use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::moonraker::{McuTransport, parse_inventory};
use aldis::plan::build_update_plan;
use aldis::prepare::{PreparationError, prepare_build};

#[test]
fn prepares_a_selected_planned_mcu_without_writing_files() {
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");
    let plan = build_update_plan(&inventory);
    let root = unique_temporary_path();
    let config_path = root.join("toolhead/.config");
    let artifact_path = root.join("artifacts/toolhead.bin");

    let prepared = prepare_build(
        &inventory,
        &plan,
        "mcu toolhead",
        config_path.clone(),
        artifact_path.clone(),
    )
    .expect("planned toolhead should prepare");

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
fn rejects_a_transport_changed_since_the_update_was_planned() {
    let planned_inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");
    let plan = build_update_plan(&planned_inventory);
    let mut discovered_inventory = planned_inventory.clone();
    discovered_inventory.mcus[1].transport = Some(McuTransport::Serial {
        device: "/dev/ttyACM0".to_owned(),
    });

    assert_eq!(
        prepare_build(
            &discovered_inventory,
            &plan,
            "mcu toolhead",
            PathBuf::from("toolhead/.config"),
            PathBuf::from("artifacts/toolhead.bin"),
        ),
        Err(PreparationError::TransportMismatch {
            target_name: "mcu toolhead".to_owned(),
            planned_transport: Some(McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            }),
            discovered_transport: Some(McuTransport::Serial {
                device: "/dev/ttyACM0".to_owned(),
            }),
        })
    );
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-preparation-{}-{nonce}", std::process::id()))
}
