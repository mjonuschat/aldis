//! The agent's wire types and the pure assessment behind `status`.

use serde::Serialize;

use crate::eligibility::{
    CheckoutRevision, Eligibility, RevisionStatus, assess_mcu, revisions_match,
};
use crate::host::{HostCheckError, check_host};
use crate::moonraker::{
    HostInfo, KlippyState, McuInventory, McuTransport, PrintState, UnreportedReason,
};

pub const API_VERSION: u32 = 1;

/// Everything `status` is derived from, gathered without taking the update lock.
#[derive(Debug, Clone)]
pub struct HostSnapshot {
    pub url_is_local: bool,
    pub klipper_unit: Option<String>,
    pub info: HostInfo,
    pub checkout_version: Option<CheckoutRevision>,
    /// Only queried when Klippy is `ready`; `None` there means the query failed.
    pub print_state: Option<PrintState>,
    pub config_error: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub host: HostSnapshot,
    /// `None` when discovery was skipped or failed.
    pub inventory: Option<McuInventory>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusBody {
    pub api_version: u32,
    pub host: HostBody,
    pub blocker: Option<Blocker>,
    pub mcus: Vec<McuEntry>,
    pub run: Option<RunBody>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostBody {
    pub klippy_state: &'static str,
    pub klippy_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub klipper_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Blocker {
    pub reason: BlockerReason,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerReason {
    NonLocalMoonraker,
    UnsupportedInstance,
    ConfigError,
    KlippyUnavailable,
    RestartPending,
    Printing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McuEntry {
    pub name: String,
    pub transport: Option<TransportBody>,
    pub running_version: Option<String>,
    pub state: McuState,
    pub message: String,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportBody {
    Serial { device: String },
    Can { interface: String, uuid: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McuState {
    Current,
    UpdateAvailable,
    Indeterminate,
    ExternallyManaged,
    UnsupportedLegacy,
    UnsupportedMcu,
    NotIdentified,
    NotResponding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Update,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunBody {
    pub run_id: String,
    pub state: RunState,
    pub messages: Vec<UpdateResponse>,
    pub result: Option<RunResultBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Finished,
}

/// One `update_response` event payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateResponse {
    pub run_id: String,
    pub mcu: Option<String>,
    pub phase: Phase,
    pub message: String,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RunResultBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Discover,
    StopKlipper,
    Build,
    EnterBootloader,
    Flash,
    Verify,
    StartKlipper,
    Reconnect,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunResultBody {
    pub outcome: Outcome,
    pub klippy_state: &'static str,
    pub klipper_left_stopped: bool,
    pub mcus: Vec<McuOutcomeBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McuOutcomeBody {
    pub name: String,
    pub outcome: McuOutcome,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McuOutcome {
    Updated,
    Failed,
    NotAttempted,
}

/// The name frontends show and send: the Klipper object name without its `mcu ` prefix.
pub fn display_name(object: &str) -> &str {
    object.strip_prefix("mcu ").unwrap_or(object)
}

pub fn klippy_state_label(state: &KlippyState) -> &'static str {
    match state {
        KlippyState::Ready => "ready",
        KlippyState::Startup => "startup",
        KlippyState::Error => "error",
        KlippyState::Shutdown => "shutdown",
        KlippyState::Disconnected => "disconnected",
    }
}

fn transport_body(transport: Option<&McuTransport>) -> Option<TransportBody> {
    transport.map(|transport| match transport {
        McuTransport::Serial { device } => TransportBody::Serial {
            device: device.clone(),
        },
        McuTransport::Can { interface, uuid } => TransportBody::Can {
            interface: interface.clone(),
            uuid: format!("{uuid:012x}"),
        },
    })
}

fn blocker(host: &HostSnapshot) -> Option<Blocker> {
    let block = |reason, message: String| Some(Blocker { reason, message });
    if !host.url_is_local {
        return block(
            BlockerReason::NonLocalMoonraker,
            "Moonraker is not on this machine; firmware updates need a local Moonraker".to_owned(),
        );
    }
    // The URL was checked above; a fixed loopback URL leaves only the unit check.
    if let Err(HostCheckError::UnsupportedInstance(unit)) =
        check_host("http://127.0.0.1", host.klipper_unit.as_deref())
    {
        return block(
            BlockerReason::UnsupportedInstance,
            format!(
                "Moonraker manages the Klipper service {unit:?}; aldis only controls \"klipper\""
            ),
        );
    }
    if host.config_error {
        return block(BlockerReason::ConfigError, host.info.state_message.clone());
    }
    if matches!(
        host.info.state,
        KlippyState::Startup | KlippyState::Disconnected
    ) {
        return block(
            BlockerReason::KlippyUnavailable,
            host.info.state_message.clone(),
        );
    }
    let software = host.info.software_version.as_deref().unwrap_or("unknown");
    match &host.checkout_version {
        Some(CheckoutRevision::Known(checkout)) if revisions_match(software, checkout) => {}
        Some(CheckoutRevision::Known(checkout)) => {
            return block(
                BlockerReason::RestartPending,
                format!(
                    "Klipper restart pending: host is running {software}, checkout is {checkout}"
                ),
            );
        }
        _ => {
            return block(
                BlockerReason::RestartPending,
                "could not read the Klipper checkout revision".to_owned(),
            );
        }
    }
    if host.info.state == KlippyState::Ready {
        match &host.print_state {
            Some(state) if state.is_idle() => {}
            Some(_) => {
                return block(
                    BlockerReason::Printing,
                    "the printer is busy; update when it is idle".to_owned(),
                );
            }
            None => {
                return block(
                    BlockerReason::Printing,
                    "could not confirm the printer is idle".to_owned(),
                );
            }
        }
    }
    None
}

/// Derives the `status` body; `run` is filled in by the service.
pub fn assess(snapshot: &Snapshot) -> StatusBody {
    let host = &snapshot.host;
    let blocker = blocker(host);
    let software = host.info.software_version.clone();
    let target = software
        .clone()
        .map_or(CheckoutRevision::Indeterminate, CheckoutRevision::Known);
    let software_label = software.as_deref().unwrap_or("unknown");
    let mut mcus = Vec::new();
    if let Some(inventory) = &snapshot.inventory {
        for mcu in &inventory.mcus {
            let running = mcu.version.as_deref().unwrap_or("unknown");
            let status = assess_mcu(mcu, &target);
            let (state, message) = match (&status.eligibility, status.revision) {
                (Eligibility::Eligible, Some(RevisionStatus::Current)) => {
                    (McuState::Current, format!("running {running}"))
                }
                (Eligibility::Eligible, Some(RevisionStatus::UpdateRequired)) => (
                    McuState::UpdateAvailable,
                    format!("running {running}, host is {software_label}"),
                ),
                (Eligibility::Eligible, _) => (
                    McuState::Indeterminate,
                    format!("cannot compare running {running} with host {software_label}"),
                ),
                (Eligibility::ExternallyManaged(app), _) => {
                    (McuState::ExternallyManaged, format!("managed by {app}"))
                }
                (Eligibility::Unsupported(reason), _) => {
                    (McuState::UnsupportedLegacy, reason.to_string())
                }
                (Eligibility::UnsupportedMcu(family), _) => (
                    McuState::UnsupportedMcu,
                    format!("{family} boards have no bootloader aldis can flash"),
                ),
            };
            let actions = if state == McuState::UpdateAvailable && blocker.is_none() {
                vec![Action::Update]
            } else {
                Vec::new()
            };
            mcus.push(McuEntry {
                name: display_name(&mcu.name).to_owned(),
                transport: transport_body(mcu.transport.as_ref()),
                running_version: mcu.version.clone(),
                state,
                message,
                actions,
            });
        }
        for unreported in &inventory.unreported {
            mcus.push(McuEntry {
                name: display_name(&unreported.name).to_owned(),
                transport: transport_body(unreported.transport.as_ref()),
                running_version: None,
                state: match unreported.reason {
                    UnreportedReason::NotIdentified => McuState::NotIdentified,
                    UnreportedReason::NotResponding => McuState::NotResponding,
                },
                message: unreported.reason.to_string(),
                actions: Vec::new(),
            });
        }
    }
    StatusBody {
        api_version: API_VERSION,
        host: HostBody {
            klippy_state: klippy_state_label(&host.info.state),
            klippy_message: host.info.state_message.clone(),
            software_version: software,
            klipper_path: host
                .info
                .klipper_path
                .as_ref()
                .map(|path| path.display().to_string()),
            checkout_version: match &host.checkout_version {
                Some(CheckoutRevision::Known(revision)) => Some(revision.clone()),
                _ => None,
            },
        },
        blocker,
        mcus,
        run: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::moonraker::{Mcu, McuTransport, UnreportedMcu};

    fn mcu(name: &str, version: &str, kconfig: &str) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: Some("Klipper".to_owned()),
            version: Some(version.to_owned()),
            mcu: "stm32h723xx".to_owned(),
            canbus_frequency_hz: None,
            transport: Some(McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            }),
            kconfig: kconfig.to_owned(),
        }
    }

    fn ready_snapshot() -> Snapshot {
        Snapshot {
            host: HostSnapshot {
                url_is_local: true,
                klipper_unit: Some("klipper".to_owned()),
                info: HostInfo {
                    state: KlippyState::Ready,
                    state_message: "Printer is ready".to_owned(),
                    software_version: Some("v0.13.0-770-gce7002bed".to_owned()),
                    klipper_path: Some("/home/pi/klipper".into()),
                },
                checkout_version: Some(CheckoutRevision::Known(
                    "v0.13.0-770-gce7002bed".to_owned(),
                )),
                print_state: Some(PrintState::Standby),
                config_error: false,
            },
            inventory: Some(McuInventory {
                mcus: vec![
                    mcu("mcu", "v0.13.0-770-gce7002bed", "CONFIG_MACH_STM32=y\n"),
                    mcu("mcu can", "v0.13.0-753-g8c29c0a8e", "CONFIG_MACH_STM32=y\n"),
                ],
                unreported: vec![UnreportedMcu {
                    name: "mcu xiao".to_owned(),
                    transport: None,
                    reason: UnreportedReason::NotResponding,
                }],
            }),
        }
    }

    #[test]
    fn reports_an_unflashable_family_without_an_update_action() {
        let mut snapshot = ready_snapshot();
        snapshot.inventory.as_mut().unwrap().mcus.push(mcu(
            "mcu samd",
            "v0.13.0-753-g8c29c0a8e",
            "CONFIG_MACH_ATSAMD=y\n",
        ));

        let body = assess(&snapshot);
        let entry = body.mcus.iter().find(|entry| entry.name == "samd").unwrap();

        assert_eq!(entry.state, McuState::UnsupportedMcu);
        assert!(entry.actions.is_empty());
        assert_eq!(
            entry.message,
            "ATSAMD boards have no bootloader aldis can flash"
        );
    }

    #[test]
    fn reports_the_documented_status_body() {
        let body = serde_json::to_value(assess(&ready_snapshot())).unwrap();

        assert_eq!(
            body,
            json!({
                "api_version": 1,
                "host": {
                    "klippy_state": "ready",
                    "klippy_message": "Printer is ready",
                    "software_version": "v0.13.0-770-gce7002bed",
                    "klipper_path": "/home/pi/klipper",
                    "checkout_version": "v0.13.0-770-gce7002bed"
                },
                "blocker": null,
                "mcus": [
                    {"name": "mcu", "transport": {"type": "can", "interface": "can0", "uuid": "e7819ed8e7d3"},
                     "running_version": "v0.13.0-770-gce7002bed", "state": "current",
                     "message": "running v0.13.0-770-gce7002bed", "actions": []},
                    {"name": "can", "transport": {"type": "can", "interface": "can0", "uuid": "e7819ed8e7d3"},
                     "running_version": "v0.13.0-753-g8c29c0a8e", "state": "update_available",
                     "message": "running v0.13.0-753-g8c29c0a8e, host is v0.13.0-770-gce7002bed",
                     "actions": ["update"]},
                    {"name": "xiao", "transport": null, "running_version": null, "state": "not_responding",
                     "message": "not responding; run `aldis reboot` or power-cycle the board", "actions": []}
                ],
                "run": null
            })
        );
    }

    #[test]
    fn blocks_in_documented_precedence_and_clears_actions() {
        let mut snapshot = ready_snapshot();
        snapshot.host.print_state = Some(PrintState::Printing);
        let body = assess(&snapshot);
        assert_eq!(
            body.blocker.as_ref().map(|b| b.reason),
            Some(BlockerReason::Printing)
        );
        assert!(body.mcus.iter().all(|m| m.actions.is_empty()));

        snapshot.host.checkout_version =
            Some(CheckoutRevision::Known("v0.13.0-771-gabcdef012".to_owned()));
        assert_eq!(
            assess(&snapshot).blocker.map(|b| b.reason),
            Some(BlockerReason::RestartPending)
        );

        snapshot.host.klipper_unit = Some("klipper-1".to_owned());
        assert_eq!(
            assess(&snapshot).blocker.map(|b| b.reason),
            Some(BlockerReason::UnsupportedInstance)
        );

        snapshot.host.url_is_local = false;
        assert_eq!(
            assess(&snapshot).blocker.map(|b| b.reason),
            Some(BlockerReason::NonLocalMoonraker)
        );
    }

    #[test]
    fn blocks_when_a_ready_printers_idleness_cannot_be_confirmed() {
        let mut snapshot = ready_snapshot();
        snapshot.host.print_state = None;
        let body = assess(&snapshot);
        assert_eq!(
            body.blocker.map(|b| (b.reason, b.message)),
            Some((
                BlockerReason::Printing,
                "could not confirm the printer is idle".to_owned()
            ))
        );

        snapshot.host.info.state = KlippyState::Error;
        assert_eq!(assess(&snapshot).blocker, None);
    }

    #[test]
    fn treats_dirty_host_and_checkout_with_the_same_base_as_matching() {
        let mut snapshot = ready_snapshot();
        snapshot.host.info.software_version = Some("v0.13.0-770-gce7002bed-dirty".to_owned());
        snapshot.host.checkout_version = Some(CheckoutRevision::Known(
            "v0.13.0-770-gce7002bed-dirty".to_owned(),
        ));
        assert_eq!(assess(&snapshot).blocker, None);
    }

    #[test]
    fn reports_config_errors_and_unavailable_klippy_without_mcus() {
        let mut snapshot = ready_snapshot();
        snapshot.inventory = None;
        snapshot.host.config_error = true;
        snapshot.host.info.state = KlippyState::Error;
        assert_eq!(
            assess(&snapshot).blocker.map(|b| b.reason),
            Some(BlockerReason::ConfigError)
        );

        snapshot.host.config_error = false;
        snapshot.host.info.state = KlippyState::Disconnected;
        let body = assess(&snapshot);
        assert_eq!(
            body.blocker.map(|b| b.reason),
            Some(BlockerReason::KlippyUnavailable)
        );
        assert!(body.mcus.is_empty());
    }

    #[test]
    fn strips_only_the_mcu_prefix_from_display_names() {
        assert_eq!(display_name("mcu"), "mcu");
        assert_eq!(display_name("mcu RP2040"), "RP2040");
    }
}
