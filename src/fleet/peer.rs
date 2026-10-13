//! The only rule between printers. A USB-CAN bridge is flashed only while every other printer on
//! its CAN bus is idle.

use std::collections::BTreeSet;

use crate::flash_order::is_usb_can_bridge;
use crate::moonraker::{KlippyState, McuInventory, McuTransport, PrintState};

/// What is known about another printer on this host. `None` means "could not be established".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    /// The Klipper unit, or the Moonraker URL when the unit is unknown.
    pub name: String,
    /// The printer's Moonraker URL.
    pub url: String,
    /// The Klipper unit, when Moonraker could be asked.
    pub unit: Option<String>,
    /// Klippy's state; `None` when Moonraker was unreachable.
    pub klippy: Option<KlippyState>,
    /// Klipper's print state; `None` when it could not be read.
    pub print_state: Option<PrintState>,
    /// The printer's MCUs; `None` when they could not be read. A printer whose Klipper failed to
    /// load its config has an empty inventory.
    pub inventory: Option<McuInventory>,
}

/// The CAN interfaces a bridge flash cuts off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bus {
    /// These interfaces.
    Interfaces(BTreeSet<String>),
    /// A bridge reached over USB serial has no knowable CAN interface, so every bus counts.
    Unknown,
}

/// The bus affected by flashing `targets`, or `None` when none of them is a USB-CAN bridge.
pub fn bridge_bus(inventory: &McuInventory, targets: &[String]) -> Option<Bus> {
    let mut interfaces = BTreeSet::new();
    let mut any = false;
    let mut unknown = false;
    for mcu in inventory
        .mcus
        .iter()
        .filter(|mcu| targets.contains(&mcu.name) && is_usb_can_bridge(mcu))
    {
        any = true;
        match &mcu.transport {
            Some(McuTransport::Can { interface, .. }) => {
                interfaces.insert(interface.clone());
            }
            _ => unknown = true,
        }
    }
    match (any, unknown) {
        (false, _) => None,
        (true, true) => Some(Bus::Unknown),
        (true, false) => Some(Bus::Interfaces(interfaces)),
    }
}

fn peer_interfaces(inventory: &McuInventory) -> BTreeSet<String> {
    let transports = inventory
        .mcus
        .iter()
        .map(|mcu| &mcu.transport)
        .chain(inventory.unreported.iter().map(|mcu| &mcu.transport));
    transports
        .filter_map(|transport| match transport {
            Some(McuTransport::Can { interface, .. }) => Some(interface.clone()),
            _ => None,
        })
        .collect()
}

/// Whether flashing `targets` is allowed given the other printers; the error says why not.
pub fn check_bridge(
    inventory: &McuInventory,
    targets: &[String],
    peers: &[PeerState],
) -> Result<(), String> {
    let Some(bus) = bridge_bus(inventory, targets) else {
        return Ok(());
    };
    for peer in peers {
        let name = &peer.name;
        let Some(klippy) = &peer.klippy else {
            return Err(format!(
                "cannot flash: {name} cannot be checked (its Moonraker is unreachable)"
            ));
        };
        match klippy {
            KlippyState::Disconnected => continue,
            KlippyState::Startup => {
                return Err(format!("cannot flash: {name} is starting up"));
            }
            _ => {}
        }
        let Some(peer_inventory) = &peer.inventory else {
            return Err(format!("cannot flash: {name} has not reported its MCUs"));
        };
        let used = peer_interfaces(peer_inventory);
        let shared = match &bus {
            Bus::Unknown => used.iter().next(),
            Bus::Interfaces(bridge) => used.iter().find(|interface| bridge.contains(*interface)),
        };
        let Some(interface) = shared else {
            continue;
        };
        let idle = matches!(klippy, KlippyState::Error | KlippyState::Shutdown)
            || peer.print_state.as_ref().is_some_and(PrintState::is_idle);
        if !idle {
            let state = peer
                .print_state
                .as_ref()
                .map_or_else(|| "not confirmed idle".to_owned(), ToString::to_string);
            return Err(format!("cannot flash: {name} is {state} on {interface}"));
        }
    }
    Ok(())
}

/// Drops the peer that is this printer (by unit) and repeats of one unit, first wins. Peers with
/// an unknown unit are kept.
pub fn peers_excluding(own_unit: &str, peers: Vec<PeerState>) -> Vec<PeerState> {
    let mut seen = BTreeSet::new();
    peers
        .into_iter()
        .filter(|peer| match &peer.unit {
            Some(unit) => unit != own_unit && seen.insert(unit.clone()),
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moonraker::{Mcu, UnreportedMcu, UnreportedReason};

    fn can_mcu(name: &str, interface: &str, uuid: u64, kconfig: &str) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: Some("Klipper".to_owned()),
            version: Some("v1".to_owned()),
            mcu: "stm32".to_owned(),
            canbus_frequency_hz: None,
            transport: Some(McuTransport::Can {
                interface: interface.to_owned(),
                uuid,
            }),
            kconfig: kconfig.to_owned(),
        }
    }

    fn inventory(mcus: Vec<Mcu>) -> McuInventory {
        McuInventory {
            mcus,
            unreported: Vec::new(),
        }
    }

    fn peer(name: &str, can_interface: &str, print: PrintState) -> PeerState {
        PeerState {
            name: name.to_owned(),
            url: format!("http://{name}.invalid:7125"),
            unit: Some(name.to_owned()),
            klippy: Some(KlippyState::Ready),
            print_state: Some(print),
            inventory: Some(inventory(vec![can_mcu(
                "mcu",
                can_interface,
                0xaa,
                "CONFIG_MACH_STM32=y\n",
            )])),
        }
    }

    fn mine() -> McuInventory {
        inventory(vec![
            can_mcu("mcu", "can0", 1, "CONFIG_USBCANBUS=y\n"),
            can_mcu("mcu toolhead", "can0", 2, "CONFIG_MACH_STM32=y\n"),
        ])
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn bridge_ok(peers: &[PeerState]) -> Result<(), String> {
        check_bridge(&mine(), &names(&["mcu"]), peers)
    }

    #[test]
    fn a_non_bridge_flash_ignores_peers() {
        let peers = [peer("klipper-2", "can0", PrintState::Printing)];
        assert_eq!(
            check_bridge(&mine(), &names(&["mcu toolhead"]), &peers),
            Ok(())
        );
    }

    #[test]
    fn idle_peers_on_the_bus_allow_the_bridge_and_busy_ones_on_other_buses_do_not_matter() {
        assert_eq!(
            bridge_ok(&[peer("klipper-2", "can0", PrintState::Standby)]),
            Ok(())
        );
        assert_eq!(
            bridge_ok(&[peer("klipper-3", "can1", PrintState::Printing)]),
            Ok(())
        );
        assert_eq!(bridge_ok(&[]), Ok(()));
    }

    #[test]
    fn a_printing_peer_on_the_bus_blocks_the_bridge_and_is_named() {
        let message = bridge_ok(&[peer("klipper-2", "can0", PrintState::Printing)]).unwrap_err();
        assert!(message.contains("klipper-2"), "{message}");
        assert!(message.contains("printing"), "{message}");
        assert!(message.contains("can0"), "{message}");
    }

    #[test]
    fn peers_in_error_or_shutdown_count_as_idle() {
        let mut broken = peer("klipper-2", "can0", PrintState::Printing);
        broken.klippy = Some(KlippyState::Shutdown);
        assert_eq!(bridge_ok(&[broken]), Ok(()));
    }

    #[test]
    fn a_peer_that_cannot_be_checked_blocks_but_one_without_klipper_does_not() {
        let mut stopped = peer("klipper-2", "can0", PrintState::Printing);
        stopped.klippy = Some(KlippyState::Disconnected);
        stopped.inventory = None;
        assert_eq!(bridge_ok(&[stopped.clone()]), Ok(()));

        let mut unreachable = stopped.clone();
        unreachable.klippy = None;
        let message = bridge_ok(&[unreachable]).unwrap_err();
        assert!(message.contains("unreachable"), "{message}");

        let mut starting = stopped.clone();
        starting.klippy = Some(KlippyState::Startup);
        assert!(bridge_ok(&[starting]).unwrap_err().contains("starting up"));

        let mut no_inventory = peer("klipper-2", "can0", PrintState::Standby);
        no_inventory.inventory = None;
        assert!(bridge_ok(&[no_inventory.clone()]).is_err());
        no_inventory.klippy = Some(KlippyState::Error);
        assert!(
            bridge_ok(&[no_inventory.clone()]).is_err(),
            "a failed query is not a config error"
        );
        no_inventory.inventory = Some(McuInventory {
            mcus: Vec::new(),
            unreported: Vec::new(),
        });
        assert_eq!(
            bridge_ok(&[no_inventory]),
            Ok(()),
            "a config error owns no MCUs"
        );
    }

    #[test]
    fn a_bridge_reached_over_usb_serial_counts_every_can_bus() {
        let mut bridge = can_mcu("mcu", "can0", 1, "CONFIG_USBCANBUS=y\n");
        bridge.transport = Some(McuTransport::Serial {
            device: "/dev/serial/by-id/usb-bridge".to_owned(),
        });
        let inventory = inventory(vec![bridge]);
        let busy = [peer("klipper-2", "can7", PrintState::Printing)];
        assert!(check_bridge(&inventory, &names(&["mcu"]), &busy).is_err());
    }

    #[test]
    fn unreported_can_mcus_still_count_as_using_the_bus() {
        let mut other = peer("klipper-2", "can9", PrintState::Printing);
        other.inventory = Some(McuInventory {
            mcus: Vec::new(),
            unreported: vec![UnreportedMcu {
                name: "mcu ghost".to_owned(),
                transport: Some(McuTransport::Can {
                    interface: "can0".to_owned(),
                    uuid: 5,
                }),
                reason: UnreportedReason::NotResponding,
            }],
        });
        assert!(bridge_ok(&[other]).is_err());
    }

    #[test]
    fn drops_ourselves_and_repeated_peers_by_unit() {
        let mut unknown = peer("klipper-9", "can0", PrintState::Standby);
        unknown.unit = None;
        let kept = peers_excluding(
            "klipper",
            vec![
                peer("klipper", "can0", PrintState::Standby),
                peer("klipper-2", "can0", PrintState::Standby),
                peer("klipper-2", "can0", PrintState::Standby),
                unknown.clone(),
                unknown,
            ],
        );
        let names: Vec<&str> = kept.iter().map(|peer| peer.name.as_str()).collect();
        assert_eq!(names, ["klipper-2", "klipper-9", "klipper-9"]);
    }
}
