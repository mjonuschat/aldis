use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::kconfig::InvalidKconfig;
use aldis::moonraker::{Mcu, McuInventory, McuTransport, parse_inventory};
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

#[test]
fn refuses_a_reported_kconfig_that_make_would_evaluate() {
    let inventory = McuInventory {
        mcus: vec![Mcu {
            name: "mcu".to_owned(),
            app: Some("Klipper".to_owned()),
            version: Some("v0.13.0-770-gce7002bed".to_owned()),
            mcu: "stm32h723xx".to_owned(),
            canbus_frequency_hz: None,
            transport: None,
            kconfig: "CONFIG_MACH_STM32=y\nCROSS_PREFIX=/tmp/evil/\n".to_owned(),
        }],
        unreported: Vec::new(),
    };

    let error = prepare_build(
        &inventory,
        "mcu",
        PathBuf::from("mcu/.config"),
        PathBuf::from("artifacts/mcu.bin"),
        false,
    )
    .expect_err("a Kconfig with a make assignment must not be prepared");

    assert_eq!(
        error,
        PreparationError::UntrustedKconfig {
            target: "mcu".to_owned(),
            source: InvalidKconfig::Line {
                line: 2,
                reason: "not a CONFIG_ symbol assignment",
                text: "CROSS_PREFIX=/tmp/evil/".to_owned(),
            },
        }
    );
    assert_eq!(
        aldis::error_chain(&error),
        "refusing to build \"mcu\" from the Kconfig its firmware reports: \
         line 2 of the MCU-reported Kconfig is not a plain Kconfig setting \
         (not a CONFIG_ symbol assignment): \"CROSS_PREFIX=/tmp/evil/\""
    );
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-preparation-{}-{nonce}", std::process::id()))
}
