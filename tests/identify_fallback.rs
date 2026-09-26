use std::cell::RefCell;

use aldis::identify::{
    IdentifyData, IdentifyError, IdentifyPort, resolve_unreported, should_probe,
};
use aldis::moonraker::{KlippyState, McuInventory, McuTransport, UnreportedMcu, UnreportedReason};

struct Scripted(RefCell<Vec<(&'static str, Result<IdentifyData, IdentifyError>)>>);

impl IdentifyPort for Scripted {
    fn identify(&self, device: &str) -> Result<IdentifyData, IdentifyError> {
        let mut script = self.0.borrow_mut();
        let index = script
            .iter()
            .position(|(d, _)| *d == device)
            .expect("unexpected probe");
        script.remove(index).1
    }
}

fn unreported(name: &str, transport: Option<McuTransport>) -> UnreportedMcu {
    UnreportedMcu {
        name: name.to_owned(),
        transport,
        reason: UnreportedReason::NotIdentified,
    }
}

fn serial(device: &str) -> Option<McuTransport> {
    Some(McuTransport::Serial {
        device: device.to_owned(),
    })
}

#[test]
fn identifies_serial_mcus_and_never_probes_can() {
    let mut inventory = McuInventory {
        mcus: Vec::new(),
        unreported: vec![
            unreported("mcu xiao", serial("/dev/xiao")),
            unreported("mcu mmb", serial("/dev/mmb")),
            unreported("mcu held", serial("/dev/held")),
            unreported(
                "mcu can",
                Some(McuTransport::Can {
                    interface: "can0".to_owned(),
                    uuid: 1,
                }),
            ),
        ],
    };
    let prober = Scripted(RefCell::new(vec![
        (
            "/dev/xiao",
            Ok(IdentifyData {
                app: None,
                version: "v0.13.0-770-gce7002bed".to_owned(),
                mcu: "samd21g18a".to_owned(),
                canbus_frequency_hz: None,
                kconfig: "CONFIG_MACH_ATSAMD=y\n".to_owned(),
            }),
        ),
        ("/dev/mmb", Err(IdentifyError::NoResponse)),
        (
            "/dev/held",
            Err(IdentifyError::PortUnavailable(
                std::io::ErrorKind::ResourceBusy.into(),
            )),
        ),
    ]));

    resolve_unreported(&mut inventory, &prober);

    assert_eq!(inventory.mcus.len(), 1);
    assert_eq!(inventory.mcus[0].name, "mcu xiao");
    assert_eq!(
        inventory.mcus[0].version.as_deref(),
        Some("v0.13.0-770-gce7002bed")
    );
    assert_eq!(inventory.mcus[0].transport, serial("/dev/xiao"));
    let reasons: Vec<_> = inventory
        .unreported
        .iter()
        .map(|m| (m.name.as_str(), m.reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("mcu can", UnreportedReason::NotIdentified),
            ("mcu held", UnreportedReason::NotIdentified),
            ("mcu mmb", UnreportedReason::NotResponding),
        ]
    );
}

#[test]
fn never_probes_the_linux_host_mcu_pipe() {
    let mut inventory = McuInventory {
        mcus: Vec::new(),
        unreported: vec![unreported("mcu host", serial("/tmp/klipper_host_mcu"))],
    };
    // No script entries: a probe of the host MCU pipe must panic via "unexpected probe" instead of
    // this test silently exercising real host MCU traffic.
    let prober = Scripted(RefCell::new(Vec::new()));

    resolve_unreported(&mut inventory, &prober);

    assert_eq!(inventory.mcus.len(), 0);
    assert_eq!(inventory.unreported.len(), 1);
    assert_eq!(inventory.unreported[0].name, "mcu host");
    assert_eq!(
        inventory.unreported[0].reason,
        UnreportedReason::NotIdentified
    );
}

#[test]
fn probes_only_in_error_or_shutdown() {
    assert!(should_probe(&KlippyState::Error));
    assert!(should_probe(&KlippyState::Shutdown));
    assert!(!should_probe(&KlippyState::Ready));
    assert!(!should_probe(&KlippyState::Startup));
    assert!(!should_probe(&KlippyState::Disconnected));
}
