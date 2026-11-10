//! The build-and-flash update loop, shared by every caller that drives a run
//! (the CLI and the Moonraker agent) behind the [`RunHooks`] trait.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::build::CommandPort;
use crate::coordinator::{BuildCoordinator, FlashCoordinatorError, UpdateProgress};
use crate::eligibility::TargetVersions;
use crate::flash::linux_host::HOST_MCU_UNIT_FILE;
use crate::flash::system::{SystemFlashOptions, SystemKatapultOptions};
use crate::foci::ReleasePort;
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

/// Waits for a freshly flashed MCU's device (serial node, or a USB-CAN bridge's CAN interface)
/// to re-enumerate. MCUs without such a device are not waited for.
pub fn wait_for_application(mcu: &crate::moonraker::Mcu) -> Result<(), String> {
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

/// Waits for Klipper to report every MCU in `selected` again, at its target version when that is
/// known; without one only reconnection is verified.
pub fn wait_for_mcus(
    client: &(impl MoonrakerPort + HostPort),
    selected: &[String],
    versions: &TargetVersions,
) -> Result<(), String> {
    wait_for_mcus_with(
        client,
        selected,
        versions,
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
    versions: &TargetVersions,
    prober: &impl crate::identify::IdentifyPort,
) -> Result<(), String> {
    wait_for_mcus_with_params(
        client,
        selected,
        versions,
        prober,
        Duration::from_secs(30),
        Duration::from_millis(250),
    )
}

fn wait_for_mcus_with_params(
    client: &(impl MoonrakerPort + HostPort),
    selected: &[String],
    versions: &TargetVersions,
    prober: &impl crate::identify::IdentifyPort,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), String> {
    retry_until_available(timeout, poll_interval, || {
        let attempt_result = match client.discover_mcus() {
            Ok(mut inventory) => {
                probe_unreported(client, &mut inventory, prober);
                match pending_mcus(&inventory, selected, versions) {
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

/// Selected MCU names not yet reporting their target version, each annotated with its
/// current state so a timeout explains what was actually observed.
fn pending_mcus(
    inventory: &McuInventory,
    selected: &[String],
    versions: &TargetVersions,
) -> Vec<String> {
    selected
        .iter()
        .filter_map(
            |name| match inventory.mcus.iter().find(|mcu| &mcu.name == name) {
                None => Some(format!("{name} (not reported by Moonraker)")),
                Some(mcu) if versions.reached_by(mcu) == Some(false) => Some(format!(
                    "{name} (reports {})",
                    mcu.version.as_deref().unwrap_or("unknown")
                )),
                Some(_) => None,
            },
        )
        .collect()
}

/// Which command a failure message speaks for; the wording differs slightly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// `aldis update`: firmware built from the checkout.
    Update,
    /// `aldis flash`: an explicit firmware file.
    Flash,
}

impl Operation {
    fn failed(self) -> &'static str {
        match self {
            Self::Update => "update failed",
            Self::Flash => "flash failed",
        }
    }

    fn firmware(self) -> &'static str {
        match self {
            Self::Update => "the built firmware",
            Self::Flash => "the firmware file",
        }
    }

    fn attempt(self) -> &'static str {
        match self {
            Self::Update => "this update",
            Self::Flash => "this flash attempt",
        }
    }
}

/// What went wrong, without any advice about Klipper's state.
pub fn failure_detail(operation: Operation, error: FlashCoordinatorError) -> String {
    match error {
        FlashCoordinatorError::Coordinator(error) => crate::error_chain(&error),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read {}: {error}", operation.firmware())
        }
        // The backends' messages are headlines ("STM32 DFU flashing failed"); the cause is in
        // the chain.
        FlashCoordinatorError::Flash(error) => crate::error_chain(&error),
    }
}

/// The terminal message for a failed `operation`: `detail`, then what state Klipper was left
/// in given whether a restore was attempted and how it went.
pub fn failure_message(
    operation: Operation,
    detail: String,
    restore: Option<Result<(), crate::coordinator::CoordinatorError>>,
) -> String {
    let failed = operation.failed();
    match restore {
        None => format!(
            "{failed}: {detail}. Klipper may still be stopped; fix the issue, then restart it manually"
        ),
        Some(Ok(())) => format!(
            "{failed}: {detail}. Klipper has been left in its state from before {}",
            operation.attempt()
        ),
        Some(Err(restore_error)) => format!(
            "{failed}: {detail}. Klipper also failed to restore: {}; fix the issue, then restart it manually",
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

/// What to update: `targets` in flashing order, each to its version in `target_versions`.
pub struct RunRequest<'a> {
    pub targets: &'a [String],
    pub target_versions: &'a TargetVersions,
    /// Where FOCI MCUs' firmware is downloaded from.
    pub foci_releases: &'a dyn ReleasePort,
    pub clean: bool,
    pub allow_downgrade: bool,
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

/// A failure after firmware was written, while Klipper is still stopped for the run and no
/// restore is attempted: the terminal message carries the same advice as a flash failure.
fn post_flash_failure(
    hooks: &mut dyn RunHooks,
    detail: String,
    mcu: Option<&str>,
    updated: &[String],
) -> RunFailure {
    fail_with_detail(
        hooks,
        detail.clone(),
        failure_message(Operation::Update, detail, None),
        mcu,
        updated,
    )
}

/// Builds (or, for FOCI, Beacon and Cartographer, downloads or reads) and flashes each approved
/// target in order, then starts
/// Klipper and waits for every updated MCU to reconnect at its target version. The first failure
/// ends the run.
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
        if !request.allow_downgrade
            && let Some(refusal) =
                crate::eligibility::downgrade_refusal(mcu, request.target_versions)
        {
            return Err(fail(hooks, refusal, Some(name), &updated));
        }
        let current = mcu.version.as_deref().unwrap_or("unknown");
        if !hooks.approve(name, current, &request.target_versions.label_for(mcu)) {
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
        let prepared = crate::prebuilt::prepare_automatic(
            mcu,
            request.target_versions,
            request.foci_releases,
            &pending,
            &mut |progress| hooks.progress(name, progress),
        );
        let flashed = match prepared {
            Some(Ok(flash_request)) => coordinator.flash_prebuilt(
                pending.approve(),
                &flash_request,
                standard_flash_options(),
                |progress| hooks.progress(name, progress),
            ),
            Some(Err(detail)) => {
                // Nothing was written to this MCU; Klipper goes back to how it was found.
                let restore = coordinator.restore_after_failure();
                return Err(fail_with_detail(
                    hooks,
                    detail.clone(),
                    failure_message(Operation::Update, detail, Some(restore)),
                    Some(name),
                    &updated,
                ));
            }
            None => coordinator
                .execute_and_flash_system_with_progress(
                    pending.approve(),
                    standard_flash_options(),
                    |progress| hooks.progress(name, progress),
                )
                .map(|completed| completed.flash),
        };
        match flashed {
            Ok(flash) => {
                hooks.flashed(name, &mcu.kconfig, flash.padded_bytes);
                hooks.step_started(RunStep::WaitForApplication);
                if let Err(message) = wait_for_application(mcu) {
                    tracing::debug!(error = %message, "waiting for MCU restart failed");
                    return Err(post_flash_failure(hooks, message, Some(name), &updated));
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
                let detail = failure_detail(Operation::Update, error);
                return Err(fail_with_detail(
                    hooks,
                    detail.clone(),
                    failure_message(Operation::Update, detail, restore),
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
        return Err(post_flash_failure(
            hooks,
            format!("could not start Klipper: {}", crate::error_chain(&error)),
            None,
            &updated,
        ));
    }
    hooks.step_succeeded(RunStep::StartKlipper);
    hooks.step_started(RunStep::Reconnect);
    if let Err(message) = wait_for_mcus(moonraker, &updated, request.target_versions) {
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
    use crate::eligibility::{CheckoutRevision, TargetVersions};
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
                python_path: None,
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

        let error = FlashCoordinatorError::Flash(SystemFlashError::LinuxHost(
            LinuxHostError::CommandFailed {
                step: InstallStep::Install,
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: b"sudo: a password is required\n".to_vec(),
                }),
            },
        ));

        let message = failure_message(
            Operation::Update,
            failure_detail(Operation::Update, error),
            None,
        );

        assert!(
            message.starts_with(
                "update failed: could not install /usr/local/bin/klipper_mcu: \
                 sudo: a password is required; run sudo aldis setup"
            ),
            "{message}"
        );
    }

    #[test]
    fn reports_the_cause_behind_a_native_flash_failure() {
        use crate::flash::katapult::bootstrap::CanBootstrapError;
        use crate::flash::katapult::can::CanTransportError;
        use crate::flash::stm32_dfu::Stm32DfuError;

        let dfu = FlashCoordinatorError::Flash(SystemFlashError::Stm32Dfu(
            Stm32DfuError::VerificationMismatch,
        ));
        assert_eq!(
            failure_detail(Operation::Update, dfu),
            "STM32 DFU flashing failed: flash readback did not match the written firmware"
        );

        let can = FlashCoordinatorError::Flash(SystemFlashError::Can(
            CanBootstrapError::Transport(CanTransportError::Timeout),
        ));
        assert_eq!(
            failure_detail(Operation::Update, can),
            "could not enter the bootloader over CAN: CAN exchange did not complete within the configured timeout"
        );
    }

    #[test]
    fn reports_build_stderr_without_debugging_command_buffers() {
        let error = FlashCoordinatorError::Coordinator(CoordinatorError::Build(
            BuildError::CommandFailed {
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
            },
        ));

        let message = failure_message(
            Operation::Update,
            failure_detail(Operation::Update, error),
            None,
        );

        assert!(message.contains("make failed: permission denied"));
        assert!(!message.contains("unrelated output"));
        assert!(message.contains("may still be stopped"));
    }

    #[test]
    fn reports_that_klipper_was_restored_after_a_pre_flash_failure() {
        let error = FlashCoordinatorError::Artifact(std::io::Error::other("missing artifact"));

        let message = failure_message(
            Operation::Update,
            failure_detail(Operation::Update, error),
            Some(Ok(())),
        );

        assert!(message.contains("left in its state from before this update"));
        assert!(!message.contains("may still be stopped"));
    }

    #[test]
    fn reports_when_restoring_klipper_after_a_pre_flash_failure_also_fails() {
        let error = FlashCoordinatorError::Artifact(std::io::Error::other("missing artifact"));
        let restore_error =
            CoordinatorError::UnexpectedKlipperState(crate::service::ServiceState::Failed);

        let message = failure_message(
            Operation::Update,
            failure_detail(Operation::Update, error),
            Some(Err(restore_error)),
        );

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
        let checkout = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()));
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
        let checkout = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()));
        let moonraker = FakeMoonraker::new(Ok(McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
            unreported: Vec::new(),
        }));

        assert!(wait_for_mcus(&moonraker, &selected, &checkout).is_ok());
    }

    #[test]
    fn wait_for_mcus_resolves_a_still_unreported_mcu_via_direct_identify() {
        let selected = vec!["mcu expander".to_owned()];
        let checkout = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()));
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
                build_versions: None,
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
        let checkout = TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()));
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
                python_path: None,
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
            build_versions: None,
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
                python_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    /// A FOCI release source that must not be asked.
    struct NoReleases;

    impl ReleasePort for NoReleases {
        fn latest_release(&self) -> Result<crate::foci::Release, crate::foci::ReleaseError> {
            panic!("runs never look up a release");
        }

        fn download_firmware(
            &self,
            _: &crate::foci::Release,
            _: &str,
        ) -> Result<Vec<u8>, crate::foci::ReleaseError> {
            panic!("no FOCI MCU is updated");
        }
    }

    /// Serves one FOCI firmware download and records the asset asked for.
    struct OneDownload {
        result: fn() -> Result<Vec<u8>, crate::foci::ReleaseError>,
        asked: RefCell<Vec<String>>,
    }

    impl ReleasePort for OneDownload {
        fn latest_release(&self) -> Result<crate::foci::Release, crate::foci::ReleaseError> {
            panic!("runs never look up a release");
        }

        fn download_firmware(
            &self,
            release: &crate::foci::Release,
            asset: &str,
        ) -> Result<Vec<u8>, crate::foci::ReleaseError> {
            self.asked
                .borrow_mut()
                .push(format!("{}/{asset}", release.tag));
            (self.result)()
        }
    }

    /// Answers every `systemctl is-active` with `inactive`, so nothing needs stopping.
    #[derive(Clone, Default)]
    struct InactiveKlipper(Arc<Mutex<Vec<BuildCommand>>>);

    impl CommandPort for InactiveKlipper {
        fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            self.0.lock().unwrap().push(command.clone());
            Ok(CommandOutput {
                success: false,
                stdout: b"inactive\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    fn foci_mcu(transport: McuTransport) -> Mcu {
        Mcu {
            name: "mcu ouroboros".to_owned(),
            app: Some("FOCI".to_owned()),
            version: Some("v0.4.0-0-g0fb68e1b".to_owned()),
            mcu: "stm32h723xx".to_owned(),
            canbus_frequency_hz: None,
            build_versions: Some("rust=1.98.1 features=none board=0fb68e1b".to_owned()),
            transport: Some(transport),
            kconfig: String::new(),
        }
    }

    fn foci_target() -> TargetVersions {
        TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned()))
            .with_foci_release(crate::foci::Release::from_tag("v0.4.1"))
    }

    fn approving() -> Recorder {
        Recorder {
            approve: true,
            ..Recorder::default()
        }
    }

    #[test]
    fn downloads_the_foci_release_image_and_sends_it_to_katapult_only() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let inventory = McuInventory {
            mcus: vec![foci_mcu(McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            })],
            unreported: Vec::new(),
        };
        let runner = InactiveKlipper::default();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        let releases = OneDownload {
            result: || Ok(vec![0xab; 16]),
            asked: RefCell::new(Vec::new()),
        };
        let mut hooks = approving();

        let failure = run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &inventory,
            &workspace,
            RunRequest {
                targets: &["mcu ouroboros".to_owned()],
                target_versions: &foci_target(),
                foci_releases: &releases,
                clean: false,
                allow_downgrade: false,
            },
            &mut hooks,
        )
        .unwrap_err();

        assert_eq!(
            *releases.asked.borrow(),
            vec!["v0.4.1/ouroboros-fw-v0.4.1-prod.bin".to_owned()]
        );
        let paths = workspace.build_paths("mcu ouroboros").unwrap();
        assert_eq!(
            std::fs::read(
                paths
                    .artifact_path
                    .with_file_name("ouroboros-fw-v0.4.1-prod.bin")
            )
            .unwrap(),
            vec![0xab; 16],
            "the run keeps the image it flashed"
        );
        assert!(
            failure
                .detail
                .contains("only through Katapult on a USB serial device"),
            "{}",
            failure.detail
        );
        assert!(
            failure
                .message
                .ends_with("Klipper has been left in its state from before this update"),
            "{}",
            failure.message
        );
        assert!(
            runner
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|command| !format!("{command:?}").contains("make")),
            "a FOCI update never builds"
        );
    }

    #[test]
    fn a_failed_foci_download_flashes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let inventory = McuInventory {
            mcus: vec![foci_mcu(McuTransport::Serial {
                device: "/dev/serial/by-id/usb-foci_FOCI_Ouroboros-if00".to_owned(),
            })],
            unreported: Vec::new(),
        };
        let runner = RecordingRunner::default();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        let releases = OneDownload {
            result: || {
                Err(crate::foci::ReleaseError::ChecksumMismatch(
                    "ouroboros-fw-v0.4.1-prod.bin".to_owned(),
                ))
            },
            asked: RefCell::new(Vec::new()),
        };
        let mut hooks = approving();

        let failure = run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &inventory,
            &workspace,
            RunRequest {
                targets: &["mcu ouroboros".to_owned()],
                target_versions: &foci_target(),
                foci_releases: &releases,
                clean: false,
                allow_downgrade: false,
            },
            &mut hooks,
        )
        .unwrap_err();

        assert_eq!(
            failure.detail,
            "could not download ouroboros-fw-v0.4.1-prod.bin from FOCI v0.4.1: downloaded \
             ouroboros-fw-v0.4.1-prod.bin does not match its published SHA-256 digest"
        );
        assert_eq!(failure.mcu.as_deref(), Some("mcu ouroboros"));
        assert!(
            runner.0.lock().unwrap().is_empty(),
            "Klipper is neither stopped nor restarted"
        );
    }

    #[test]
    fn refuses_a_foci_update_without_a_known_release() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let inventory = McuInventory {
            mcus: vec![foci_mcu(McuTransport::Serial {
                device: "/dev/serial/by-id/usb-foci_FOCI_Ouroboros-if00".to_owned(),
            })],
            unreported: Vec::new(),
        };
        let runner = RecordingRunner::default();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        let mut hooks = approving();

        let failure = run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &inventory,
            &workspace,
            RunRequest {
                targets: &["mcu ouroboros".to_owned()],
                target_versions: &TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
                foci_releases: &NoReleases,
                clean: false,
                allow_downgrade: false,
            },
            &mut hooks,
        )
        .unwrap_err();

        assert_eq!(
            failure.detail,
            "no FOCI release is known to update mcu ouroboros to"
        );
        assert!(runner.0.lock().unwrap().is_empty());
    }

    /// CRC-32 without its final inversion, as a DFU suffix stores it.
    fn dfu_crc(bytes: &[u8]) -> u32 {
        bytes.iter().fold(u32::MAX, |crc, &byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        })
    }

    fn beacon_image_file(bcd: u16) -> Vec<u8> {
        let mut file = vec![0x5a; 32];
        for word in [bcd, 0xbea1, 0x04d8, 0x0100] {
            file.extend_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(b"UFD");
        file.push(16);
        let crc = dfu_crc(&file);
        file.extend_from_slice(&crc.to_le_bytes());
        file
    }

    fn beacon_mcu(transport: McuTransport) -> Mcu {
        Mcu {
            name: "mcu beacon".to_owned(),
            app: Some(String::new()),
            version: Some("Beacon 2.0.1".to_owned()),
            mcu: "samd21g18a".to_owned(),
            canbus_frequency_hz: None,
            build_versions: None,
            transport: Some(transport),
            kconfig: String::new(),
        }
    }

    /// Targets with `mcu beacon` updated to the image at `path`, said to hold `bcd`.
    fn beacon_targets(path: &Path, bcd: u16) -> TargetVersions {
        TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())).with_beacon_targets(
            crate::beacon::Targets {
                checkout: path.parent().map(Path::to_owned),
                mcus: [(
                    "mcu beacon".to_owned(),
                    Ok(crate::beacon::Target {
                        revision: "h".to_owned(),
                        path: path.to_owned(),
                        version: crate::beacon::version_from_bcd(bcd),
                    }),
                )]
                .into(),
            },
        )
    }

    fn run_beacon(
        transport: McuTransport,
        versions: &TargetVersions,
        runner: &InactiveKlipper,
        workspace: &RunWorkspace,
    ) -> RunFailure {
        let inventory = McuInventory {
            mcus: vec![beacon_mcu(transport)],
            unreported: Vec::new(),
        };
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner.clone());
        run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &inventory,
            workspace,
            RunRequest {
                targets: &["mcu beacon".to_owned()],
                target_versions: versions,
                foci_releases: &NoReleases,
                clean: false,
                allow_downgrade: false,
            },
            &mut approving(),
        )
        .unwrap_err()
    }

    #[test]
    fn reads_the_beacon_image_from_the_checkout_and_flashes_it_over_usb_only() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let image = dir.path().join("revh.dfu");
        std::fs::write(&image, beacon_image_file(0x2010)).unwrap();
        let runner = InactiveKlipper::default();

        let failure = run_beacon(
            McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            },
            &beacon_targets(&image, 0x2010),
            &runner,
            &workspace,
        );

        let paths = workspace.build_paths("mcu beacon").unwrap();
        assert_eq!(
            std::fs::read(paths.artifact_path.with_file_name("revh.dfu")).unwrap(),
            beacon_image_file(0x2010),
            "the run keeps the image it flashed"
        );
        assert!(
            failure
                .detail
                .starts_with("Beacon firmware is flashed only over USB"),
            "{}",
            failure.detail
        );
        assert!(
            failure
                .message
                .ends_with("Klipper has been left in its state from before this update"),
            "{}",
            failure.message
        );
        assert!(
            runner
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|command| !format!("{command:?}").contains("make")),
            "a Beacon update never builds"
        );
    }

    #[test]
    fn refuses_a_beacon_image_that_changed_since_it_was_assessed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let image = dir.path().join("revh.dfu");
        std::fs::write(&image, beacon_image_file(0x2020)).unwrap();
        let runner = InactiveKlipper::default();

        let failure = run_beacon(
            McuTransport::Serial {
                device: "/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00".to_owned(),
            },
            &beacon_targets(&image, 0x2010),
            &runner,
            &workspace,
        );

        assert_eq!(
            failure.detail,
            format!(
                "{} now holds Beacon 2.2.0, not the Beacon 2.1.0 mcu beacon was to be updated to",
                image.display()
            )
        );
        assert_eq!(failure.mcu.as_deref(), Some("mcu beacon"));
        assert!(
            failure
                .message
                .ends_with("Klipper has been left in its state from before this update"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn refuses_a_beacon_update_without_a_readable_image() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let serial = McuTransport::Serial {
            device: "/dev/serial/by-id/usb-Beacon_Beacon_RevH-if00".to_owned(),
        };

        let missing = run_beacon(
            serial.clone(),
            &beacon_targets(&dir.path().join("revh.dfu"), 0x2010),
            &InactiveKlipper::default(),
            &workspace,
        );
        assert!(
            missing.detail.starts_with(&format!(
                "could not read {}",
                dir.path().join("revh.dfu").display()
            )),
            "{}",
            missing.detail
        );

        let unknown = run_beacon(
            serial,
            &TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
            &InactiveKlipper::default(),
            &workspace,
        );
        assert_eq!(
            unknown.detail,
            "no Beacon firmware image is known to update mcu beacon with"
        );
    }

    /// A Cartographer application image: a vector table above the 8 KiB Katapult, then the
    /// zlib-compressed identify dictionary.
    fn cartographer_image(version: &str, can: Option<u64>) -> Vec<u8> {
        use std::io::Write;
        let mut image = Vec::new();
        image.extend_from_slice(&0x2000_8000_u32.to_le_bytes());
        image.extend_from_slice(&0x0800_2101_u32.to_le_bytes());
        image.resize(0x200, 0);
        let mut config = serde_json::json!({ "MCU": "stm32g431xx" });
        if let Some(can) = can {
            config["CANBUS_FREQUENCY"] = can.into();
        }
        let dictionary = serde_json::json!({ "version": version, "config": config });
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(dictionary.to_string().as_bytes())
            .unwrap();
        image.extend(encoder.finish().unwrap());
        image
    }

    fn cartographer_mcu(can: Option<u64>) -> Mcu {
        Mcu {
            name: "mcu cartographer".to_owned(),
            app: Some("Cartographer".to_owned()),
            version: Some("CARTOGRAPHER V4 6.0.0".to_owned()),
            mcu: "stm32g431xx".to_owned(),
            canbus_frequency_hz: can,
            build_versions: Some(String::new()),
            transport: Some(match can {
                // An interface no host has, so the flash fails opening it.
                Some(_) => McuTransport::Can {
                    interface: "aldistest0".to_owned(),
                    uuid: 0xe781_9ed8_e7d3,
                },
                None => McuTransport::Serial {
                    device: "/dev/serial/by-id/usb-Cartographer_614e-if00".to_owned(),
                },
            }),
            kconfig: String::new(),
        }
    }

    fn cartographer_targets(path: &Path, reported: &str) -> TargetVersions {
        TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())).with_cartographer_targets(
            crate::cartographer::Targets {
                checkout: path.parent().map(Path::to_owned),
                plugin: None,
                mcus: [(
                    "mcu cartographer".to_owned(),
                    Ok(crate::cartographer::Target {
                        path: path.to_owned(),
                        version: crate::cartographer::parse_version(reported).unwrap(),
                        reported: reported.to_owned(),
                        held_back: None,
                    }),
                )]
                .into(),
            },
        )
    }

    fn run_cartographer(
        can: Option<u64>,
        versions: &TargetVersions,
        workspace: &RunWorkspace,
    ) -> RunFailure {
        let runner = InactiveKlipper::default();
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner);
        run_updates(
            &coordinator,
            &Printer(PrintState::Standby),
            &McuInventory {
                mcus: vec![cartographer_mcu(can)],
                unreported: Vec::new(),
            },
            workspace,
            RunRequest {
                targets: &["mcu cartographer".to_owned()],
                target_versions: versions,
                foci_releases: &NoReleases,
                clean: false,
                allow_downgrade: false,
            },
            &mut approving(),
        )
        .unwrap_err()
    }

    #[test]
    fn reads_the_cartographer_image_and_sends_it_to_katapult_over_can() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let image = dir.path().join("can_1m_full.bin");
        std::fs::write(
            &image,
            cartographer_image("CARTOGRAPHER v4 6.2.0", Some(1_000_000)),
        )
        .unwrap();

        let failure = run_cartographer(
            Some(1_000_000),
            &cartographer_targets(&image, "CARTOGRAPHER v4 6.2.0"),
            &workspace,
        );

        let paths = workspace.build_paths("mcu cartographer").unwrap();
        assert_eq!(
            std::fs::read(paths.artifact_path.with_file_name("can_1m_full.bin")).unwrap(),
            std::fs::read(&image).unwrap(),
            "the run keeps the image it flashed"
        );
        assert!(
            failure
                .detail
                .starts_with("could not open the configured CAN interface"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn refuses_a_cartographer_image_that_changed_or_does_not_suit_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let image = dir.path().join("can_1m_full.bin");

        std::fs::write(
            &image,
            cartographer_image("CARTOGRAPHER v4 6.2.0 Lite", Some(1_000_000)),
        )
        .unwrap();
        let changed = run_cartographer(
            Some(1_000_000),
            &cartographer_targets(&image, "CARTOGRAPHER v4 6.2.0"),
            &workspace,
        );
        assert_eq!(
            changed.detail,
            format!(
                "{} now holds \"CARTOGRAPHER v4 6.2.0 Lite\", not the \"CARTOGRAPHER v4 6.2.0\" \
                 mcu cartographer was to be updated to",
                image.display()
            )
        );
        assert!(
            changed
                .message
                .ends_with("Klipper has been left in its state from before this update"),
            "{}",
            changed.message
        );

        std::fs::write(
            &image,
            cartographer_image("CARTOGRAPHER v4 6.2.0", Some(500_000)),
        )
        .unwrap();
        let wrong_bitrate = run_cartographer(
            Some(1_000_000),
            &cartographer_targets(&image, "CARTOGRAPHER v4 6.2.0"),
            &workspace,
        );
        assert_eq!(
            wrong_bitrate.detail,
            format!(
                "refusing {} for mcu cartographer: it is built for CAN at 500000 bit/s, and the \
                 probe runs CAN at 1000000 bit/s",
                image.display()
            )
        );

        let unknown = run_cartographer(None, &TargetVersions::indeterminate(), &workspace);
        assert_eq!(
            unknown.detail,
            "no Cartographer firmware image is known to update mcu cartographer with"
        );
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
    fn post_flash_failures_tell_the_user_klipper_may_still_be_stopped() {
        let mut hooks = Recorder::default();

        let failure = post_flash_failure(
            &mut hooks,
            "mcu h723 did not come back at /dev/serial/by-id/x".to_owned(),
            Some("mcu h723"),
            &["mcu toolhead".to_owned()],
        );

        assert_eq!(
            failure.message,
            "update failed: mcu h723 did not come back at /dev/serial/by-id/x. \
             Klipper may still be stopped; fix the issue, then restart it manually"
        );
        assert_eq!(
            failure.detail,
            "mcu h723 did not come back at /dev/serial/by-id/x"
        );
        assert_eq!(failure.mcu.as_deref(), Some("mcu h723"));
        assert_eq!(failure.updated, vec!["mcu toolhead".to_owned()]);
        assert_eq!(hooks.failures, vec![failure.detail.clone()]);
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
                target_versions: &TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
                foci_releases: &NoReleases,
                clean: false,
                allow_downgrade: false,
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
                target_versions: &TargetVersions::klipper(CheckoutRevision::Known("v2".to_owned())),
                foci_releases: &NoReleases,
                clean: false,
                allow_downgrade: false,
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

    // ----- Event-order characterization -----
    //
    // The order of hooks, downloads and commands in a run, frozen so that sharing the code that
    // acquires and flashes an image cannot move a download after Klipper is stopped, skip the
    // printer check, or reorder what a caller sees.

    type Log = Arc<Mutex<Vec<String>>>;

    /// Logs every command and keeps a Klipper unit that `stop` and `start` really change.
    #[derive(Clone)]
    struct LoggingRunner {
        log: Log,
        active: Arc<Mutex<bool>>,
    }

    impl CommandPort for LoggingRunner {
        fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            self.log.lock().unwrap().push(format!(
                "command: {} {}",
                command.program,
                command.arguments.join(" ")
            ));
            let has = |word: &str| command.arguments.iter().any(|argument| argument == word);
            let mut active = self.active.lock().unwrap();
            if has("stop") {
                *active = false;
            } else if has("start") {
                *active = true;
            }
            let asking = has("is-active");
            Ok(CommandOutput {
                success: !asking || *active,
                stdout: match (asking, *active) {
                    (true, true) => b"active\n".to_vec(),
                    (true, false) => b"inactive\n".to_vec(),
                    _ => Vec::new(),
                },
                stderr: Vec::new(),
            })
        }
    }

    struct LogHooks {
        log: Log,
        approve: bool,
    }

    impl RunHooks for LogHooks {
        fn approve(&mut self, name: &str, current: &str, next: &str) -> bool {
            self.log
                .lock()
                .unwrap()
                .push(format!("approve {name}: {current} -> {next}"));
            self.approve
        }
        fn progress(&mut self, name: &str, progress: UpdateProgress) {
            self.log
                .lock()
                .unwrap()
                .push(format!("progress {name}: {progress:?}"));
        }
        fn flashed(&mut self, name: &str, _: &str, bytes: usize) {
            self.log
                .lock()
                .unwrap()
                .push(format!("flashed {name}: {bytes}"));
        }
        fn step_started(&mut self, step: RunStep) {
            self.log
                .lock()
                .unwrap()
                .push(format!("step started {step:?}"));
        }
        fn step_succeeded(&mut self, step: RunStep) {
            self.log
                .lock()
                .unwrap()
                .push(format!("step succeeded {step:?}"));
        }
        fn failed(&mut self, message: &str) {
            self.log.lock().unwrap().push(format!("failed: {message}"));
        }
    }

    /// A FOCI release source that logs the asset it is asked for.
    struct LoggingReleases {
        log: Log,
        corrupt: bool,
    }

    impl ReleasePort for LoggingReleases {
        fn latest_release(&self) -> Result<crate::foci::Release, crate::foci::ReleaseError> {
            panic!("runs never look up a release");
        }

        fn download_firmware(
            &self,
            _: &crate::foci::Release,
            asset: &str,
        ) -> Result<Vec<u8>, crate::foci::ReleaseError> {
            self.log.lock().unwrap().push(format!("download {asset}"));
            if self.corrupt {
                Err(crate::foci::ReleaseError::ChecksumMismatch(
                    asset.to_owned(),
                ))
            } else {
                Ok(vec![0xab; 16])
            }
        }
    }

    struct Scenario {
        mcu: Mcu,
        versions: TargetVersions,
        state: PrintState,
        klipper_active: bool,
        approve: bool,
        corrupt_download: bool,
    }

    /// Runs `scenario` and returns the ordered log, ending with the failure detail and message.
    fn trace(scenario: Scenario) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let workspace = RunWorkspace::create(dir.path().join("run")).unwrap();
        let log = Log::default();
        let runner = LoggingRunner {
            log: log.clone(),
            active: Arc::new(Mutex::new(scenario.klipper_active)),
        };
        let coordinator = BuildCoordinator::new("/nonexistent", runner.clone(), runner);
        let releases = LoggingReleases {
            log: log.clone(),
            corrupt: scenario.corrupt_download,
        };
        let name = scenario.mcu.name.clone();
        let mut hooks = LogHooks {
            log: log.clone(),
            approve: scenario.approve,
        };
        let result = run_updates(
            &coordinator,
            &Printer(scenario.state),
            &McuInventory {
                mcus: vec![scenario.mcu],
                unreported: Vec::new(),
            },
            &workspace,
            RunRequest {
                targets: &[name],
                target_versions: &scenario.versions,
                foci_releases: &releases,
                clean: false,
                allow_downgrade: false,
            },
            &mut hooks,
        );
        let mut events = log.lock().unwrap().clone();
        match result {
            Ok(outcome) => events.push(format!("outcome: updated {:?}", outcome.updated)),
            Err(failure) => {
                events.push(format!("detail: {}", failure.detail));
                events.push(format!("message: {}", failure.message));
                events.push(format!(
                    "mcu: {:?}, updated {:?}",
                    failure.mcu, failure.updated
                ));
            }
        }
        events
    }

    fn foci_scenario(transport: McuTransport) -> Scenario {
        Scenario {
            mcu: foci_mcu(transport),
            versions: foci_target(),
            state: PrintState::Standby,
            klipper_active: true,
            approve: true,
            corrupt_download: false,
        }
    }

    fn foci_can() -> McuTransport {
        McuTransport::Can {
            interface: "can0".to_owned(),
            uuid: 0xe781_9ed8_e7d3,
        }
    }

    #[test]
    fn traces_a_foci_download_then_a_refused_link_with_klipper_running() {
        let events = trace(foci_scenario(foci_can()));
        assert_eq!(events, [
                "approve mcu ouroboros: v0.4.0-0-g0fb68e1b -> v0.4.1",
                "progress mcu ouroboros: DownloadingFirmware",
                "download ouroboros-fw-v0.4.1-prod.bin",
                "progress mcu ouroboros: FirmwareDownloaded",
                "command: sudo -n /bin/systemctl is-active klipper",
                "progress mcu ouroboros: StoppingKlipper",
                "command: sudo -n /bin/systemctl stop klipper",
                "command: sudo -n /bin/systemctl is-active klipper",
                "command: sudo -n /bin/systemctl start klipper",
                "command: sudo -n /bin/systemctl is-active klipper",
                "failed: this firmware is flashed only through Katapult on a USB serial device, and the MCU is configured on CAN (can0, e7819ed8e7d3)",
                "detail: this firmware is flashed only through Katapult on a USB serial device, and the MCU is configured on CAN (can0, e7819ed8e7d3)",
                "message: update failed: this firmware is flashed only through Katapult on a USB serial device, and the MCU is configured on CAN (can0, e7819ed8e7d3). Klipper has been left in its state from before this update",
                "mcu: Some(\"mcu ouroboros\"), updated []",
            ].map(String::from).to_vec());
    }

    #[test]
    fn traces_a_corrupt_foci_download_with_klipper_running() {
        let mut scenario = foci_scenario(foci_can());
        scenario.corrupt_download = true;
        assert_eq!(trace(scenario), [
                "approve mcu ouroboros: v0.4.0-0-g0fb68e1b -> v0.4.1",
                "progress mcu ouroboros: DownloadingFirmware",
                "download ouroboros-fw-v0.4.1-prod.bin",
                "failed: could not download ouroboros-fw-v0.4.1-prod.bin from FOCI v0.4.1: downloaded ouroboros-fw-v0.4.1-prod.bin does not match its published SHA-256 digest",
                "detail: could not download ouroboros-fw-v0.4.1-prod.bin from FOCI v0.4.1: downloaded ouroboros-fw-v0.4.1-prod.bin does not match its published SHA-256 digest",
                "message: update failed: could not download ouroboros-fw-v0.4.1-prod.bin from FOCI v0.4.1: downloaded ouroboros-fw-v0.4.1-prod.bin does not match its published SHA-256 digest. Klipper has been left in its state from before this update",
                "mcu: Some(\"mcu ouroboros\"), updated []",
            ].map(String::from).to_vec());
    }

    #[test]
    fn traces_a_busy_printer_and_a_declined_approval() {
        let mut busy = foci_scenario(foci_can());
        busy.state = PrintState::Printing;
        assert_eq!(trace(busy), [
                "approve mcu ouroboros: v0.4.0-0-g0fb68e1b -> v0.4.1",
                "failed: the printer is printing; refusing to stop Klipper, run again when it is idle",
                "detail: the printer is printing; refusing to stop Klipper, run again when it is idle",
                "message: the printer is printing; refusing to stop Klipper, run again when it is idle",
                "mcu: Some(\"mcu ouroboros\"), updated []",
            ].map(String::from).to_vec());

        let mut declined = foci_scenario(foci_can());
        declined.approve = false;
        assert_eq!(
            trace(declined),
            [
                "approve mcu ouroboros: v0.4.0-0-g0fb68e1b -> v0.4.1",
                "outcome: updated []",
            ]
            .map(String::from)
            .to_vec()
        );
    }

    #[test]
    fn refuses_to_downgrade_any_firmware_before_asking_or_touching_klipper() {
        const REFUSAL: &str = "mcu beacon runs Beacon 2.2.0, newer than the Beacon 2.1.0 in its beacon_klipper checkout, and an update never downgrades it: name the file with `aldis flash beacon <file>` to install the older image";
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("revh.dfu");
        std::fs::write(&image, beacon_image_file(0x2010)).unwrap();
        let mut scenario = foci_scenario(foci_can());
        scenario.mcu = beacon_mcu(foci_can());
        scenario.mcu.version = Some("Beacon 2.2.0".to_owned());
        scenario.versions = beacon_targets(&image, 0x2010);

        assert_eq!(
            trace(scenario),
            [
                format!("failed: {REFUSAL}"),
                format!("detail: {REFUSAL}"),
                format!("message: {REFUSAL}"),
                "mcu: Some(\"mcu beacon\"), updated []".to_owned(),
            ],
            "nothing is asked, downloaded or run"
        );
    }

    #[test]
    fn traces_a_beacon_and_a_cartographer_update_that_stop_at_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let beacon = dir.path().join("revh.dfu");
        std::fs::write(&beacon, beacon_image_file(0x2010)).unwrap();
        let mut scenario = foci_scenario(foci_can());
        scenario.mcu = beacon_mcu(foci_can());
        scenario.versions = beacon_targets(&beacon, 0x2010);
        assert_eq!(trace(scenario), [
                "approve mcu beacon: Beacon 2.0.1 -> Beacon 2.1.0",
                "progress mcu beacon: ReadingFirmware",
                "progress mcu beacon: FirmwareRead",
                "command: sudo -n /bin/systemctl is-active klipper",
                "progress mcu beacon: StoppingKlipper",
                "command: sudo -n /bin/systemctl stop klipper",
                "command: sudo -n /bin/systemctl is-active klipper",
                "command: sudo -n /bin/systemctl start klipper",
                "command: sudo -n /bin/systemctl is-active klipper",
                "failed: Beacon firmware is flashed only over USB, and the Beacon is configured on CAN (can0, e7819ed8e7d3)",
                "detail: Beacon firmware is flashed only over USB, and the Beacon is configured on CAN (can0, e7819ed8e7d3)",
                "message: update failed: Beacon firmware is flashed only over USB, and the Beacon is configured on CAN (can0, e7819ed8e7d3). Klipper has been left in its state from before this update",
                "mcu: Some(\"mcu beacon\"), updated []",
            ].map(String::from).to_vec());

        let image = dir.path().join("can_1m_full.bin");
        std::fs::write(
            &image,
            cartographer_image("CARTOGRAPHER v4 6.2.0", Some(1_000_000)),
        )
        .unwrap();
        let mut scenario = foci_scenario(foci_can());
        scenario.mcu = cartographer_mcu(Some(1_000_000));
        scenario.versions = cartographer_targets(&image, "CARTOGRAPHER v4 6.2.0");
        assert_eq!(trace(scenario), [
                "approve mcu cartographer: CARTOGRAPHER V4 6.0.0 -> CARTOGRAPHER v4 6.2.0",
                "progress mcu cartographer: ReadingFirmware",
                "progress mcu cartographer: FirmwareRead",
                "command: sudo -n /bin/systemctl is-active klipper",
                "progress mcu cartographer: StoppingKlipper",
                "command: sudo -n /bin/systemctl stop klipper",
                "command: sudo -n /bin/systemctl is-active klipper",
                "progress mcu cartographer: EnteringBootloader",
                "failed: could not open the configured CAN interface: No such device (os error 19)",
                "detail: could not open the configured CAN interface: No such device (os error 19)",
                "message: update failed: could not open the configured CAN interface: No such device (os error 19). Klipper may still be stopped; fix the issue, then restart it manually",
                "mcu: Some(\"mcu cartographer\"), updated []",
            ].map(String::from).to_vec());
    }
}
