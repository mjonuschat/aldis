use std::path::PathBuf;

use aldis::flash::katapult::endpoint::{EndpointError, KatapultEndpoint, endpoint_for};
use aldis::moonraker::McuTransport;
use aldis::prepare::PreparedBuild;

fn prepared(transport: Option<McuTransport>) -> PreparedBuild {
    PreparedBuild {
        target_name: "mcu toolhead".to_owned(),
        mcu: "stm32g0b1xx".to_owned(),
        transport,
        request: aldis::build::BuildRequest {
            kconfig: String::new(),
            config_path: PathBuf::from("toolhead/.config"),
            artifact_path: PathBuf::from("artifacts/toolhead.bin"),
            clean: false,
        },
    }
}

#[test]
fn retains_the_explicit_serial_device_without_resolving_a_bootloader_path() {
    let endpoint = endpoint_for(&prepared(Some(McuTransport::Serial {
        device: "/dev/serial/by-id/usb-Klipper_stm32f429xx_3200-if00".to_owned(),
    })))
    .unwrap();

    assert_eq!(
        endpoint,
        KatapultEndpoint::Serial {
            running_device: PathBuf::from("/dev/serial/by-id/usb-Klipper_stm32f429xx_3200-if00"),
        }
    );
}

#[test]
fn retains_the_explicit_can_interface_and_uuid() {
    let endpoint = endpoint_for(&prepared(Some(McuTransport::Can {
        interface: "can0".to_owned(),
        uuid: 0xe781_9ed8_e7d3,
    })))
    .unwrap();

    assert_eq!(
        endpoint,
        KatapultEndpoint::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        }
    );
}

#[test]
fn rejects_a_target_without_a_configured_transport() {
    assert_eq!(
        endpoint_for(&prepared(None)),
        Err(EndpointError::MissingTransport {
            target_name: "mcu toolhead".to_owned(),
        })
    );
}
