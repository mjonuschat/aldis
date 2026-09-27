//! The build-and-flash update loop, shared by every caller that drives a run
//! (the CLI and the Moonraker agent) behind the [`RunHooks`] trait.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::build::CommandPort;
use crate::coordinator::{BuildCoordinator, FlashCoordinatorError, UpdateProgress};
use crate::eligibility::{CheckoutRevision, revisions_match};
use crate::flash::katapult::system::SystemKatapultOptions;
use crate::flash::linux_host::HOST_MCU_UNIT_FILE;
use crate::flash::system::{SystemFlashError, SystemFlashOptions};
use crate::moonraker::{HostPort, McuInventory, McuTransport, MoonrakerPort, PrinterStatePort};
use crate::retry::retry_until_available;
use crate::workspace::RunWorkspace;

pub fn ensure_printer_idle(printer: &(impl PrinterStatePort + HostPort)) -> Result<(), String> {
    if printer.host_info().is_ok_and(|info| {
        matches!(
            info.state,
            crate::moonraker::KlippyState::Error | crate::moonraker::KlippyState::Shutdown
        )
    }) {
        return Ok(());
    }
    match printer.print_state() {
        Ok(state) if state.is_idle() => Ok(()),
        Ok(state) => Err(format!(
            "the printer is {state}; refusing to stop Klipper, run again when it is idle"
        )),
        Err(error) => Err(format!(
            "could not confirm the printer is idle; refusing to stop Klipper: {}",
            crate::error_chain(&error)
        )),
    }
}

const NET_SYSFS_ROOT: &str = "/sys/class/net";

fn wait_for_application(mcu: &crate::moonraker::Mcu) -> Result<(), String> {
    wait_for_application_in(mcu, Duration::from_secs(15), Path::new(NET_SYSFS_ROOT))
}

fn wait_for_application_in(
    mcu: &crate::moonraker::Mcu,
    timeout: Duration,
    net_root: &Path,
) -> Result<(), String> {
    let (path, description) = match &mcu.transport {
        Some(McuTransport::Serial { device }) => (PathBuf::from(device), device.clone()),
        Some(McuTransport::Can { interface, .. }) if crate::flash_order::is_usb_can_bridge(mcu) => {
            (
                net_root.join(interface),
                format!("CAN interface {interface}"),
            )
        }
        _ => return Ok(()),
    };
    retry_until_available(timeout, Duration::from_millis(100), || {
        path.exists().then_some(()).ok_or(()).inspect_err(|()| {
            tracing::debug!(path = %path.display(), "mcu not yet back, still waiting");
        })
    })
    .map_err(|()| format!("{} did not come back at {description}", mcu.name))
}

fn wait_for_mcus(
    client: &(impl MoonrakerPort + HostPort),
    selected: &[String],
    checkout: &CheckoutRevision,
) -> Result<(), String> {
    wait_for_mcus_with(
        client,
        selected,
        checkout,
        &crate::identify::SerialIdentify::default(),
    )
}

/// [`wait_for_mcus`] with an injectable direct-identify prober.
///
/// A freshly flashed MCU can come back from Moonraker as unreported again: Klipper's restart
/// re-runs config load in the same order, so it still stops at whatever unrelated, unreachable
/// predecessor MCU stopped it before the flash. Running the same fallback used during discovery
/// against each poll lets a genuinely successful flash confirm itself even though Klippy never
/// reaches that MCU.
fn wait_for_mcus_with(
    client: &(impl MoonrakerPort + HostPort),
    selected: &[String],
    checkout: &CheckoutRevision,
    prober: &impl crate::identify::IdentifyPort,
) -> Result<(), String> {
    wait_for_mcus_with_params(
        client,
        selected,
        checkout,
        prober,
        Duration::from_secs(30),
        Duration::from_millis(250),
    )
}

fn wait_for_mcus_with_params(
    client: &(impl MoonrakerPort + HostPort),
    selected: &[String],
    checkout: &CheckoutRevision,
    prober: &impl crate::identify::IdentifyPort,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), String> {
    retry_until_available(timeout, poll_interval, || {
        let attempt_result = match client.discover_mcus() {
            Ok(mut inventory) => {
                probe_unreported(client, &mut inventory, prober);
                match pending_mcus(&inventory, selected, checkout) {
                    pending if pending.is_empty() => Ok(()),
                    pending => Err(format!("still waiting on: {}", pending.join(", "))),
                }
            }
            Err(error) => Err(format!("could not query Moonraker: {error}")),
        };
        if let Err(ref error) = attempt_result {
            tracing::debug!(error = %error, "mcus not yet ready, still waiting");
        }
        attempt_result
    })
    .map_err(|last_state| {
        format!("Klipper did not reconnect every updated MCU at the built revision ({last_state})")
    })
}

/// Runs the direct-identify fallback when Klippy is in an error state. The caller has already
/// verified, once, at the start of the whole run, that this Moonraker fronts the local, default
/// Klipper service, so unlike `discovery::probe_unreported` there is no per-poll host re-check
/// here.
fn probe_unreported(
    moonraker: &impl HostPort,
    inventory: &mut McuInventory,
    prober: &impl crate::identify::IdentifyPort,
) {
    if inventory.unreported.is_empty() {
        return;
    }
    if moonraker
        .host_info()
        .is_ok_and(|info| crate::identify::should_probe(&info.state))
    {
        crate::identify::resolve_unreported(inventory, prober);
    }
}

/// Selected MCU names not yet reporting `checkout`'s revision, each annotated with its
/// current state so a timeout explains what was actually observed.
fn pending_mcus(
    inventory: &McuInventory,
    selected: &[String],
    checkout: &CheckoutRevision,
) -> Vec<String> {
    selected
        .iter()
        .filter_map(
            |name| match inventory.mcus.iter().find(|mcu| &mcu.name == name) {
                None => Some(format!("{name} (not reported by Moonraker)")),
                Some(mcu) => match checkout {
                    CheckoutRevision::Known(revision)
                        if !mcu
                            .version
                            .as_deref()
                            .is_some_and(|version| revisions_match(version, revision)) =>
                    {
                        Some(format!(
                            "{name} (reports {})",
                            mcu.version.as_deref().unwrap_or("unknown")
                        ))
                    }
                    _ => None,
                },
            },
        )
        .collect()
}

fn failure_detail(error: FlashCoordinatorError<SystemFlashError>) -> String {
    match error {
        FlashCoordinatorError::Coordinator(error) => crate::error_chain(&error),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read the built firmware: {error}")
        }
        FlashCoordinatorError::Flash(error) => error.to_string(),
    }
}

fn update_failure_message(
    detail: String,
    restore: Option<Result<(), crate::coordinator::CoordinatorError>>,
) -> String {
    match restore {
        None => format!(
            "update failed: {detail}. Klipper may still be stopped; fix the issue, then restart it manually"
        ),
        Some(Ok(())) => format!(
            "update failed: {detail}. Klipper has been left in its state from before this update"
        ),
        Some(Err(restore_error)) => format!(
            "update failed: {detail}. Klipper also failed to restore: {}; fix the issue, then restart it manually",
            crate::error_chain(&restore_error)
        ),
    }
}

/// A labelled step after a flash, reported so callers can show progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStep {
    WaitForApplication,
    StartKlipper,
    Reconnect,
}

/// How a caller approves each MCU and observes a run.
pub trait RunHooks {
    /// Whether to update `name` from `current` to `next`.
    fn approve(&mut self, name: &str, current: &str, next: &str) -> bool;
    fn progress(&mut self, name: &str, progress: UpdateProgress);
    fn flashed(&mut self, name: &str, kconfig: &str, padded_bytes: usize);
    fn step_started(&mut self, step: RunStep);
    fn step_succeeded(&mut self, step: RunStep);
    /// The run is ending with `message`; no further hooks follow.
    fn failed(&mut self, message: &str);
}

/// What to update: `targets` in flashing order, all against `target_revision`.
pub struct RunRequest<'a> {
    pub targets: &'a [String],
    pub target_revision: &'a CheckoutRevision,
    pub clean: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunOutcome {
    pub updated: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunFailure {
    /// The full message for a terminal user, including Klipper restart advice.
    pub message: String,
    /// What went wrong, without any advice about Klipper's state.
    pub detail: String,
    /// The MCU being processed when the run failed, if any.
    pub mcu: Option<String>,
    /// MCUs flashed successfully before the failure.
    pub updated: Vec<String>,
}

/// The flash backend settings every caller uses.
pub fn standard_flash_options() -> SystemFlashOptions {
    SystemFlashOptions {
        katapult: SystemKatapultOptions {
            baud_rate: 250_000,
            bootloader_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(50),
            read_timeout: Duration::from_secs(5),
            can_bootloader_settle: Duration::from_millis(100),
        },
        host_mcu_unit_file: PathBuf::from(HOST_MCU_UNIT_FILE),
    }
}

fn revision_label(revision: &CheckoutRevision) -> &str {
    match revision {
        CheckoutRevision::Known(revision) => revision,
        CheckoutRevision::Indeterminate => "unknown",
    }
}

fn fail(
    hooks: &mut dyn RunHooks,
    message: String,
    mcu: Option<&str>,
    updated: &[String],
) -> RunFailure {
    fail_with_detail(hooks, message.clone(), message, mcu, updated)
}

fn fail_with_detail(
    hooks: &mut dyn RunHooks,
    detail: String,
    message: String,
    mcu: Option<&str>,
    updated: &[String],
) -> RunFailure {
    hooks.failed(&detail);
    RunFailure {
        message,
        detail,
        mcu: mcu.map(str::to_owned),
        updated: updated.to_vec(),
    }
}

/// Builds and flashes each approved target in order, then starts Klipper and waits for every
/// updated MCU to reconnect at `target_revision`. The first failure ends the run.
pub fn run_updates<B: CommandPort, S: CommandPort>(
    coordinator: &BuildCoordinator<B, S>,
    moonraker: &(impl MoonrakerPort + PrinterStatePort + HostPort),
    inventory: &McuInventory,
    workspace: &RunWorkspace,
    request: RunRequest<'_>,
    hooks: &mut dyn RunHooks,
) -> Result<RunOutcome, RunFailure> {
    let mut updated = Vec::new();
    let mut printer_checked = false;
    for name in request.targets {
        let Some(mcu) = inventory.mcus.iter().find(|mcu| &mcu.name == name) else {
            return Err(fail(
                hooks,
                format!("{name} is not in the discovered inventory"),
                Some(name),
                &updated,
            ));
        };
        let current = mcu.version.as_deref().unwrap_or("unknown");
        if !hooks.approve(name, current, revision_label(request.target_revision)) {
            continue;
        }
        let pending = match coordinator.prepare(inventory, workspace, name, request.clean) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::debug!(?error, "flash preparation failed");
                let message = format!(
                    "failed to prepare {name} for flashing: {}",
                    crate::error_chain(&error)
                );
                return Err(fail(hooks, message, Some(name), &updated));
            }
        };
        if !printer_checked {
            if let Err(message) = ensure_printer_idle(moonraker) {
                return Err(fail(hooks, message, Some(name), &updated));
            }
            printer_checked = true;
        }
        match coordinator.execute_and_flash_system_with_progress(
            pending.approve(),
            standard_flash_options(),
            |progress| hooks.progress(name, progress),
        ) {
            Ok(completed) => {
                hooks.flashed(name, &mcu.kconfig, completed.flash.padded_bytes);
                hooks.step_started(RunStep::WaitForApplication);
                if let Err(message) = wait_for_application(mcu) {
                    tracing::debug!(error = %message, "waiting for MCU restart failed");
                    return Err(fail(hooks, message, Some(name), &updated));
                }
                hooks.step_succeeded(RunStep::WaitForApplication);
                updated.push(name.clone());
            }
            Err(error) => {
                tracing::debug!(?error, "flash failed");
                let restore = error
                    .allows_klipper_restore()
                    .then(|| coordinator.restore_after_failure());
                if let Some(Err(restore_error)) = &restore {
                    tracing::debug!(?restore_error, "restoring Klipper after the failure failed");
                }
                let detail = failure_detail(error);
                return Err(fail_with_detail(
                    hooks,
                    detail.clone(),
                    update_failure_message(detail, restore),
                    Some(name),
                    &updated,
                ));
            }
        }
    }
    if updated.is_empty() {
        return Ok(RunOutcome { updated });
    }
    hooks.step_started(RunStep::StartKlipper);
    if let Err(error) = coordinator.start_after_batch() {
        tracing::debug!(?error, "starting Klipper after batch failed");
        return Err(fail(hooks, crate::error_chain(&error), None, &updated));
    }
    hooks.step_succeeded(RunStep::StartKlipper);
    hooks.step_started(RunStep::Reconnect);
    if let Err(message) = wait_for_mcus(moonraker, &updated, request.target_revision) {
        tracing::debug!(error = %message, "waiting for updated MCUs to reconnect failed");
        return Err(fail(hooks, message, None, &updated));
    }
    hooks.step_succeeded(RunStep::Reconnect);
    Ok(RunOutcome { updated })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::build::{BuildCommand, BuildError, CommandError, CommandOutput};
    use crate::coordinator::{BuildCoordinator, CoordinatorError, FlashCoordinatorError};
    use crate::eligibility::CheckoutRevision;
    use crate::flash::system::SystemFlashError;
    use crate::moonraker::{
        HostInfo, KlippyState, Mcu, McuInventory, McuTransport, MoonrakerError, MoonrakerPort,
        PrintState, PrinterStatePort,
    };
    use crate::workspace::RunWorkspace;

    struct FakePrinter(Result<PrintState, String>, KlippyState);

    impl FakePrinter {
        fn in_state(state: KlippyState, print: Result<PrintState, String>) -> Self {
            Self(print, state)
        }
    }

    impl PrinterStatePort for FakePrinter {
        fn print_state(&self) -> Result<PrintState, MoonrakerError> {
            self.0.clone().map_err(MoonrakerError::InvalidResponse)
        }
    }

    impl HostPort for FakePrinter {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Ok(None)
        }

        fn host_info(&self) -> Result<HostInfo, MoonrakerError> {
            Ok(HostInfo {
                state: self.1.clone(),
                state_message: String::new(),
                software_version: None,
                klipper_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    #[test]
    fn refuses_to_stop_klipper_while_a_print_is_running_or_paused() {
        for (state, label) in [
            (PrintState::Printing, "printing"),
            (PrintState::Paused, "paused"),
        ] {
            let message =
                ensure_printer_idle(&FakePrinter(Ok(state), KlippyState::Ready)).unwrap_err();

            assert_eq!(
                message,
                format!(
                    "the printer is {label}; refusing to stop Klipper, run again when it is idle"
                )
            );
        }
    }

    #[test]
    fn refuses_to_stop_klipper_when_the_print_state_is_unrecognized() {
        let message = ensure_printer_idle(&FakePrinter(
            Ok(PrintState::Unknown("resuming".to_owned())),
            KlippyState::Ready,
        ))
        .unwrap_err();

        assert!(
            message.starts_with("the printer is resuming; refusing"),
            "{message}"
        );
    }

    #[test]
    fn refuses_to_stop_klipper_when_the_print_state_cannot_be_read() {
        let message = ensure_printer_idle(&FakePrinter(
            Err("connection reset".to_owned()),
            KlippyState::Ready,
        ))
        .unwrap_err();

        assert!(
            message.starts_with("could not confirm the printer is idle; refusing to stop Klipper"),
            "{message}"
        );
        assert!(message.contains("connection reset"), "{message}");
    }

    #[test]
    fn proceeds_when_the_printer_is_idle() {
        for state in [
            PrintState::Standby,
            PrintState::Complete,
            PrintState::Cancelled,
            PrintState::Error,
        ] {
            assert!(ensure_printer_idle(&FakePrinter(Ok(state), KlippyState::Ready)).is_ok());
        }
    }

    #[test]
    fn skips_the_print_check_while_klipper_is_in_an_error_state() {
        for state in [KlippyState::Error, KlippyState::Shutdown] {
            let printer = FakePrinter::in_state(state, Err("print_stats unavailable".to_owned()));
            assert!(ensure_printer_idle(&printer).is_ok());
        }
        let ready = FakePrinter::in_state(KlippyState::Ready, Err("connection reset".to_owned()));
        assert!(ensure_printer_idle(&ready).is_err());
    }

    #[test]
    fn reports_the_host_mcu_setup_hint_in_the_final_update_error() {
        use crate::flash::linux_host::{InstallStep, LinuxHostError};

        let error = FlashCoordinatorError::<SystemFlashError>::Flash(SystemFlashError::LinuxHost(
            LinuxHostError::CommandFailed {
                step: InstallStep::Install,
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: b"sudo: a password is required\n".to_vec(),
                }),
            },
        ));

        let message = update_failure_message(failure_detail(error), None);

        assert!(
            message.starts_with(
                "update failed: could not install /usr/local/bin/klipper_mcu: \
                 sudo: a password is required; run sudo aldis setup"
            ),
            "{message}"
        );
    }

    #[test]
    fn reports_build_stderr_without_debugging_command_buffers() {
        let error = FlashCoordinatorError::<SystemFlashError>::Coordinator(
            CoordinatorError::Build(BuildError::CommandFailed {
                command: Box::new(BuildCommand {
                    program: "make".to_owned(),
                    arguments: Vec::new(),
                    current_dir: None,
                    stdin: None,
                }),
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: b"unrelated output".to_vec(),
                    stderr: b"permission denied".to_vec(),
                }),
            }),
        );

        let message = update_failure_message(failure_detail(error), None);

        assert!(message.contains("make failed: permission denied"));
        assert!(!message.contains("unrelated output"));
        assert!(message.contains("may still be stopped"));
    }

    #[test]
    fn reports_that_klipper_was_restored_after_a_pre_flash_failure() {
        let error = FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other(
            "missing artifact",
        ));

        let message = update_failure_message(failure_detail(error), Some(Ok(())));

        assert!(message.contains("left in its state from before this update"));
        assert!(!message.contains("may still be stopped"));
    }

    #[test]
    fn reports_when_restoring_klipper_after_a_pre_flash_failure_also_fails() {
        let error = FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other(
            "missing artifact",
        ));
        let restore_error =
            CoordinatorError::UnexpectedKlipperState(crate::service::ServiceState::Failed);

        let message = update_failure_message(failure_detail(error), Some(Err(restore_error)));

        assert!(message.contains("also failed to restore"));
        assert!(message.contains("restart it manually"));
    }

    #[test]
    fn rejects_a_serial_mcu_that_does_not_reenumerate() {
        let mcu = mcu("mcu h723", "v1", Some("/definitely/missing"));
        assert!(wait_for_application_in(&mcu, Duration::ZERO, Path::new(NET_SYSFS_ROOT)).is_err());
    }

    #[test]
    fn waits_for_a_flashed_bridges_can_interface() {
        let bridge = Mcu {
            transport: Some(McuTransport::Can {
                interface: "can9".to_owned(),
                uuid: 1,
            }),
            kconfig: "CONFIG_USBCANBUS=y\n".to_owned(),
            ..mcu("mcu", "v1", None)
        };
        let net = tempfile::tempdir().unwrap();

        assert!(wait_for_application_in(&bridge, Duration::ZERO, net.path()).is_err());
        std::fs::create_dir(net.path().join("can9")).unwrap();
        assert!(wait_for_application_in(&bridge, Duration::ZERO, net.path()).is_ok());
    }

    #[test]
    fn requires_every_selected_mcu_at_the_built_revision() {
        let selected = vec!["mcu h723".to_owned(), "mcu rp2040".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let only_one = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
            unreported: Vec::new(),
        };
        assert_eq!(
            pending_mcus(&only_one, &selected, &checkout),
            vec!["mcu rp2040 (not reported by Moonraker)".to_owned()]
        );
        let wrong_version = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v1", None)],
            unreported: Vec::new(),
        };
        assert_eq!(
            pending_mcus(&wrong_version, &selected, &checkout),
            vec!["mcu rp2040 (reports v1)".to_owned()]
        );
        let complete = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v2", None)],
            unreported: Vec::new(),
        };
        assert!(pending_mcus(&complete, &selected, &checkout).is_empty());
    }

    #[test]
    fn wait_for_mcus_succeeds_once_the_injected_source_reports_readiness() {
        let selected = vec!["mcu h723".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let moonraker = FakeMoonraker::new(Ok(McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
            unreported: Vec::new(),
        }));

        assert!(wait_for_mcus(&moonraker, &selected, &checkout).is_ok());
    }

    #[test]
    fn wait_for_mcus_resolves_a_still_unreported_mcu_via_direct_identify() {
        let selected = vec!["mcu expander".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let moonraker = FakeMoonraker::in_state(
            Ok(McuInventory {
                mcus: Vec::new(),
                unreported: vec![unreported_mcu("mcu expander", "/dev/expander")],
            }),
            KlippyState::Error,
        );
        let prober = ScriptedIdentify(RefCell::new(vec![(
            "/dev/expander",
            Ok(crate::identify::IdentifyData {
                app: None,
                version: "v2".to_owned(),
                mcu: "test".to_owned(),
                canbus_frequency_hz: None,
                kconfig: "CONFIG_TEST=y\n".to_owned(),
            }),
        )]));

        let result = wait_for_mcus_with_params(
            &moonraker,
            &selected,
            &checkout,
            &prober,
            Duration::ZERO,
            Duration::from_millis(10),
        );

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn wait_for_mcus_still_fails_when_direct_identify_cannot_resolve_the_mcu() {
        let selected = vec!["mcu expander".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let moonraker = FakeMoonraker::in_state(
            Ok(McuInventory {
                mcus: Vec::new(),
                unreported: vec![unreported_mcu("mcu expander", "/dev/expander")],
            }),
            KlippyState::Error,
        );
        let prober = ScriptedIdentify(RefCell::new(vec![(
            "/dev/expander",
            Err(crate::identify::IdentifyError::NoResponse),
        )]));

        let result = wait_for_mcus_with_params(
            &moonraker,
            &selected,
            &checkout,
            &prober,
            Duration::ZERO,
            Duration::from_millis(10),
        );

        let error = result.unwrap_err();
        assert!(error.contains("mcu expander"), "{error}");
    }

    struct FakeMoonraker {
        inventory: Result<McuInventory, String>,
        state: KlippyState,
    }

    impl FakeMoonraker {
        fn new(inventory: Result<McuInventory, String>) -> Self {
            Self::in_state(inventory, KlippyState::Ready)
        }

        fn in_state(inventory: Result<McuInventory, String>, state: KlippyState) -> Self {
            Self { inventory, state }
        }
    }

    impl MoonrakerPort for FakeMoonraker {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            self.inventory
                .clone()
                .map_err(MoonrakerError::InvalidResponse)
        }
    }

    impl HostPort for FakeMoonraker {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Ok(None)
        }

        fn host_info(&self) -> Result<HostInfo, MoonrakerError> {
            Ok(HostInfo {
                state: self.state.clone(),
                state_message: String::new(),
                software_version: None,
                klipper_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    impl PrinterStatePort for FakeMoonraker {
        fn print_state(&self) -> Result<PrintState, MoonrakerError> {
            Ok(PrintState::Standby)
        }
    }

    struct ScriptedIdentify(
        RefCell<
            Vec<(
                &'static str,
                Result<crate::identify::IdentifyData, crate::identify::IdentifyError>,
            )>,
        >,
    );

    impl crate::identify::IdentifyPort for ScriptedIdentify {
        fn identify(
            &self,
            device: &str,
        ) -> Result<crate::identify::IdentifyData, crate::identify::IdentifyError> {
            let mut script = self.0.borrow_mut();
            let index = script
                .iter()
                .position(|(d, _)| *d == device)
                .expect("unexpected probe");
            script.remove(index).1
        }
    }

    fn unreported_mcu(name: &str, device: &str) -> crate::moonraker::UnreportedMcu {
        crate::moonraker::UnreportedMcu {
            name: name.to_owned(),
            transport: Some(McuTransport::Serial {
                device: device.to_owned(),
            }),
            reason: crate::moonraker::UnreportedReason::NotIdentified,
        }
    }

    fn mcu(name: &str, version: &str, serial: Option<&str>) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: None,
            version: Some(version.to_owned()),
            mcu: "test".to_owned(),
            canbus_frequency_hz: None,
            transport: serial.map(|device| McuTransport::Serial {
                device: device.to_owned(),
            }),
            kconfig: "CONFIG_TEST=y\n".to_owned(),
        }
    }

    #[derive(Clone, Default)]
    struct RecordingRunner(Arc<Mutex<Vec<BuildCommand>>>);

    impl CommandPort for RecordingRunner {
        fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            self.0.lock().unwrap().push(command.clone());
            Err(CommandError::Spawn(std::io::Error::other(
                "no commands expected",
            )))
        }
    }

    #[derive(Default)]
    struct Recorder {
        approve: bool,
        failures: Vec<String>,
    }

    impl RunHooks for Recorder {
        fn approve(&mut self, _: &str, _: &str, _: &str) -> bool {
            self.approve
        }
        fn progress(&mut self, _: &str, _: UpdateProgress) {}
        fn flashed(&mut self, _: &str, _: &str, _: usize) {}
        fn step_started(&mut self, _: RunStep) {}
        fn step_succeeded(&mut self, _: RunStep) {}
        fn failed(&mut self, message: &str) {
            self.failures.push(message.to_owned());
        }
    }

    struct Printer(PrintState);

    impl PrinterStatePort for Printer {
        fn print_state(&self) -> Result<PrintState, MoonrakerError> {
            Ok(self.0.clone())
        }
    }

    impl MoonrakerPort for Printer {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            Err(MoonrakerError::KlippyNotConnected)
        }
    }

    impl HostPort for Printer {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Ok(None)
        }
        fn host_info(&self) -> Result<HostInfo, MoonrakerError> {
            Ok(HostInfo {
                state: KlippyState::Ready,
                state_message: String::new(),
                software_version: None,
                klipper_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        RunWorkspace,
        McuInventory,
        RecordingRunner,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let inventory = McuInventory {
            mcus: vec![mcu("mcu", "v1", Some("/dev/null"))],
            unreported: Vec::new(),
        };
        (dir, workspace, inventory, RecordingRunner::default())
    }

    #[test]
    fn skips_declined_mcus_without_touching_klipper() {
        let (_dir, workspace, inventory, runner) = fixture();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        let mut hooks = Recorder::default();

        let outcome = run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &inventory,
            &workspace,
            RunRequest {
                targets: &["mcu".to_owned()],
                target_revision: &CheckoutRevision::Known("v2".to_owned()),
                clean: false,
            },
            &mut hooks,
        );

        assert_eq!(outcome, Ok(RunOutcome::default()));
        assert!(runner.0.lock().unwrap().is_empty());
    }

    #[test]
    fn refuses_to_stop_klipper_while_printing() {
        let (_dir, workspace, inventory, runner) = fixture();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        let mut hooks = Recorder {
            approve: true,
            ..Recorder::default()
        };

        let failure = run_updates(
            &coordinator,
            &Printer(PrintState::Printing),
            &inventory,
            &workspace,
            RunRequest {
                targets: &["mcu".to_owned()],
                target_revision: &CheckoutRevision::Known("v2".to_owned()),
                clean: false,
            },
            &mut hooks,
        )
        .unwrap_err();

        assert!(
            failure.message.starts_with("the printer is printing"),
            "{}",
            failure.message
        );
        assert_eq!(failure.mcu.as_deref(), Some("mcu"));
        assert_eq!(hooks.failures, vec![failure.detail.clone()]);
        assert!(runner.0.lock().unwrap().is_empty());
    }
}
