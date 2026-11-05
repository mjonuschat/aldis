use aldis::eligibility::{Eligibility, classify_mcu};
use aldis::moonraker::{
    KlippyState, McuTransport, UnreportedMcu, UnreportedReason, parse_host_info, parse_inventory,
    parse_logs_root,
};

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

#[test]
fn matches_settings_case_insensitively_against_the_object_name() {
    // Klipper lowercases config section names in configfile.settings, but
    // printer.objects.list preserves the case the section was declared with
    // (e.g. `[mcu RP2040]`).
    let response = r#"{
      "result": {
        "status": {
          "mcu RP2040": {
            "app": "Klipper",
            "mcu_version": "v0.12.0",
            "mcu_constants": {
              "MCU": "rp2040"
            },
            "mcu_kconfig": "CONFIG_LOW_LEVEL_OPTIONS=y\n"
          },
          "configfile": {
            "settings": {
              "mcu rp2040": {
                "serial": "/dev/serial/by-id/usb-Klipper_rp2040_50547747686A651C-if00"
              }
            }
          }
        }
      }
    }"#;

    let inventory = parse_inventory(response).expect("fixture should parse");

    assert_eq!(
        inventory.mcus[0].transport,
        Some(McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Klipper_rp2040_50547747686A651C-if00".to_owned(),
        })
    );
}

#[test]
fn accepts_an_mcu_object_with_no_kconfig_as_unsupported_rather_than_failing() {
    let response = r#"{
      "result": {
        "status": {
          "mcu beacon": {
            "app": "Beacon",
            "mcu_version": "v1.2.3",
            "mcu_constants": {
              "MCU": "rp2040"
            }
          },
          "configfile": {
            "settings": {}
          }
        }
      }
    }"#;

    let inventory =
        parse_inventory(response).expect("a missing mcu_kconfig should not fail discovery");

    assert_eq!(inventory.mcus.len(), 1);
    assert_eq!(inventory.mcus[0].kconfig, "");
    assert_eq!(
        classify_mcu(&inventory.mcus[0]),
        Eligibility::ExternallyManaged("Beacon".to_owned())
    );
}

#[test]
fn reads_the_build_versions_an_mcu_reports() {
    let response = r#"{
      "result": {
        "status": {
          "mcu ouroboros": {
            "app": "FOCI",
            "mcu_version": "v0.4.1-0-ga82de21d",
            "mcu_build_versions": "rust=1.98.1 features=none board=a82de21d",
            "mcu_constants": {
              "MCU": "stm32h723xx"
            }
          },
          "mcu": {
            "app": "Klipper",
            "mcu_version": "v0.13.0-770-gce7002bed",
            "mcu_constants": {
              "MCU": "stm32f429xx"
            },
            "mcu_kconfig": "CONFIG_MACH_STM32=y\n"
          },
          "configfile": {
            "settings": {}
          }
        }
      }
    }"#;

    let inventory = parse_inventory(response).expect("fixture should parse");

    let build_versions = |name: &str| {
        inventory
            .mcus
            .iter()
            .find(|mcu| mcu.name == name)
            .and_then(|mcu| mcu.build_versions.clone())
    };
    assert_eq!(
        build_versions("mcu ouroboros").as_deref(),
        Some("rust=1.98.1 features=none board=a82de21d")
    );
    assert_eq!(build_versions("mcu"), None);
}

#[test]
fn keeps_empty_mcu_objects_as_unreported_with_their_configured_transport() {
    let response = r#"{
      "result": {
        "status": {
          "mcu": {
            "mcu_version": "v0.13.0-770-gce7002bed",
            "mcu_constants": {"MCU": "stm32h723xx"},
            "mcu_kconfig": "CONFIG_MACH_STM32=y\n"
          },
          "mcu XIAO": {},
          "configfile": {
            "settings": {
              "mcu xiao": {"serial": "/dev/serial/by-id/usb-Klipper_samd21g18a_1-if00"}
            }
          }
        }
      }
    }"#;

    let inventory = parse_inventory(response).expect("fixture should parse");

    assert_eq!(inventory.mcus.len(), 1);
    assert_eq!(
        inventory.unreported,
        vec![UnreportedMcu {
            name: "mcu XIAO".to_owned(),
            transport: Some(McuTransport::Serial {
                device: "/dev/serial/by-id/usb-Klipper_samd21g18a_1-if00".to_owned(),
            }),
            reason: UnreportedReason::NotIdentified,
        }]
    );
}

#[test]
fn still_rejects_a_populated_object_without_an_mcu_constant() {
    let response = r#"{"result":{"status":{"mcu":{"mcu_version":"v1","mcu_constants":{}}}}}"#;
    assert!(parse_inventory(response).is_err());
}

#[test]
fn parses_host_info_including_version_and_path() {
    let response = r#"{"result":{"state":"error","state_message":"MCU Protocol error",
        "software_version":"v0.13.0-770-gce7002bed","klipper_path":"/home/pi/klipper",
        "python_path":"/home/pi/klippy-env/bin/python"}}"#;

    let info = parse_host_info(response).unwrap();

    assert_eq!(info.state, KlippyState::Error);
    assert_eq!(info.state_message, "MCU Protocol error");
    assert_eq!(
        info.software_version.as_deref(),
        Some("v0.13.0-770-gce7002bed")
    );
    assert_eq!(
        info.klipper_path,
        Some(std::path::PathBuf::from("/home/pi/klipper"))
    );
    assert_eq!(
        info.python_path,
        Some(std::path::PathBuf::from("/home/pi/klippy-env/bin/python"))
    );

    let disconnected =
        r#"{"result":{"state":"disconnected","state_message":"Klippy Disconnected"}}"#;
    assert_eq!(
        parse_host_info(disconnected).unwrap().state,
        KlippyState::Disconnected
    );
}

#[test]
fn reads_the_logs_root_from_file_roots() {
    let response = r#"{"result":[{"name":"config","path":"/home/pi/printer_data/config","permissions":"rw"},
        {"name":"logs","path":"/home/pi/printer_data/logs","permissions":"r"}]}"#;
    assert_eq!(
        parse_logs_root(response).unwrap(),
        Some(std::path::PathBuf::from("/home/pi/printer_data/logs"))
    );
    assert_eq!(parse_logs_root(r#"{"result":[]}"#).unwrap(), None);
}

#[test]
fn reads_the_serial_port_from_a_beacon_section() {
    let response = r#"{
      "result": {
        "status": {
          "mcu beacon": {
            "app": "",
            "mcu_version": "Beacon 2.1.0",
            "mcu_constants": { "MCU": "samd21g18a" }
          },
          "mcu beacon sensor rear": {
            "app": "",
            "mcu_version": "Beacon 2.1.0",
            "mcu_constants": { "MCU": "samd21g18a" }
          },
          "configfile": {
            "settings": {
              "beacon": { "serial": "/dev/serial/by-id/usb-Beacon_Beacon_RevH_ABC-if00" },
              "beacon sensor rear": { "serial": "/dev/serial/by-id/usb-Beacon_Beacon_RevD_DEF-if00" }
            }
          }
        }
      }
    }"#;

    let inventory = parse_inventory(response).expect("fixture should parse");
    let transport = |name: &str| {
        inventory
            .mcus
            .iter()
            .find(|mcu| mcu.name == name)
            .unwrap()
            .transport
            .clone()
    };

    assert_eq!(
        transport("mcu beacon"),
        Some(McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Beacon_Beacon_RevH_ABC-if00".to_owned(),
        })
    );
    assert_eq!(
        transport("mcu beacon sensor rear"),
        Some(McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Beacon_Beacon_RevD_DEF-if00".to_owned(),
        })
    );
}

#[test]
fn describes_each_transport_the_way_users_are_told_about_it() {
    use aldis::moonraker::McuTransport;

    assert_eq!(
        McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Klipper_stm32-if00".to_owned()
        }
        .describe(),
        "serial (/dev/serial/by-id/usb-Klipper_stm32-if00)"
    );
    assert_eq!(
        McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3
        }
        .describe(),
        "CAN (can0, e7819ed8e7d3)"
    );
}

#[test]
fn reads_the_app_and_version_names_a_firmware_reports() {
    let mcu = |app: Option<&str>, version: Option<&str>| aldis::moonraker::Mcu {
        name: "mcu".to_owned(),
        app: app.map(str::to_owned),
        version: version.map(str::to_owned),
        mcu: "test".to_owned(),
        canbus_frequency_hz: None,
        build_versions: None,
        transport: None,
        kconfig: String::new(),
    };

    assert_eq!(mcu(Some(" Klipper "), None).app_name(), Some("Klipper"));
    assert_eq!(
        mcu(Some("  "), None).app_name(),
        None,
        "blank counts as absent"
    );
    assert_eq!(mcu(None, None).app_name(), None);
    assert_eq!(
        mcu(None, Some("Beacon 2.1.0")).version_name(),
        Some("Beacon")
    );
    assert_eq!(mcu(None, Some("  ")).version_name(), None);
    assert_eq!(mcu(None, None).version_name(), None);
}
