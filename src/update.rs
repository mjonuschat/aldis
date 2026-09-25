//! The interactive build-and-flash update command.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use aldis::coordinator::{BuildCoordinator, FlashCoordinatorError};
use aldis::eligibility::{
    CheckoutRevision, Eligibility, UpdateSelection, assess_mcu, is_selected, revisions_match,
};
use aldis::flash::katapult::system::SystemKatapultOptions;
use aldis::flash::linux_host::HOST_MCU_UNIT_FILE;
use aldis::flash::system::{SystemFlashError, SystemFlashOptions};
use aldis::logging::{self, LoggingCommandAdapter};
use aldis::moonraker::{
    McuInventory, McuTransport, MoonrakerAdapter, MoonrakerPort, PrinterStatePort,
};
use aldis::retry::retry_until_available;
use aldis::workspace::RunWorkspace;

use crate::cli::UpdateArgs;
use crate::fail;
use crate::status::{checkout_label, format_status, refresh_label};
use crate::ui::UpdateUi;

pub(crate) fn update(arguments: UpdateArgs, verbose: u8, mut ui: UpdateUi) -> ExitCode {
    match update_and_report_run_log(arguments, verbose, &mut ui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(format!("{error:#}")),
    }
}

fn update_and_report_run_log(
    arguments: UpdateArgs,
    verbose: u8,
    ui: &mut UpdateUi,
) -> anyhow::Result<()> {
    let workspace = match &arguments.workspace {
        Some(path) => RunWorkspace::create(path.clone())?,
        None => {
            let path =
                crate::default_run_workspace().context("could not create the run workspace")?;
            RunWorkspace::adopt(path)
        }
    };
    let run_log_path = workspace.root().join("run.log");
    let _guard = logging::init(verbose, &run_log_path).context("could not start logging")?;
    // The run log is reported exactly once, here, rather than at every
    // failure site, so it's always the last thing printed regardless of
    // which step failed.
    run_update(arguments, ui, &workspace, &run_log_path)
        .map_err(|error| anyhow::anyhow!("{error:#}\nRun log: {}", run_log_path.display()))
}

fn run_update(
    mut arguments: UpdateArgs,
    ui: &mut UpdateUi,
    workspace: &RunWorkspace,
    run_log_path: &Path,
) -> anyhow::Result<()> {
    let source = arguments
        .connection
        .klipper_source
        .unwrap_or_else(crate::default_klipper_source);
    ui.action(&format!(
        "updating from Klipper source {}",
        source.display()
    ));
    let refreshed =
        if arguments.auto || arguments.pull || (io::stdin().is_terminal() && confirm_pull()) {
            ui.action("refreshing configured Klipper upstream");
            let refreshed = refresh_checkout(&source)
                .inspect_err(|error| {
                    tracing::debug!(?error, "checkout refresh failed");
                    ui.action(&format!("error: {}", aldis::error_chain(error)));
                })
                .context("failed to refresh the configured Klipper checkout")?;
            ui.action(&format!("checkout refresh: {}", refresh_label(&refreshed)));
            Some(refreshed)
        } else {
            None
        };
    ui.action("discovering MCUs from Moonraker");
    let inventory = crate::discovery::discover_mcus_with_retry(&MoonrakerAdapter::new(
        &arguments.connection.moonraker.moonraker,
    ))
    .inspect_err(|error| {
        tracing::debug!(?error, "Moonraker discovery failed");
        ui.action(&format!("error: {}", aldis::error_chain(error)));
    })
    .context("failed to discover MCUs from Moonraker")?;
    for target in &mut arguments.targets {
        *target = crate::discovery::resolve_target_name(&inventory, target);
    }
    let unknown = unknown_targets(&inventory, &arguments.targets);
    if !unknown.is_empty() {
        anyhow::bail!(
            "unknown target MCU{}: {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown.join(", ")
        );
    }
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    ui.block(&format_status(
        &source,
        &checkout,
        &inventory,
        refreshed.as_ref(),
    ));
    let selection = selection_for(arguments.all);
    let offered = inventory
        .mcus
        .iter()
        .filter(|mcu| arguments.targets.is_empty() || arguments.targets.contains(&mcu.name))
        .filter(|mcu| {
            if arguments.force {
                assess_mcu(mcu, &checkout).eligibility == Eligibility::Eligible
            } else {
                is_selected(mcu, &checkout, &selection)
            }
        })
        .map(|mcu| mcu.name.clone())
        .collect::<Vec<_>>();
    if offered.is_empty() {
        ui.block("\nno eligible MCUs require an update\n");
        ui.action("run completed successfully: no eligible MCUs require an update");
        return Ok(());
    }
    let coordinator = BuildCoordinator::new(
        &source,
        LoggingCommandAdapter::new(SystemCommandAdapter),
        LoggingCommandAdapter::new(SystemCommandAdapter),
    );
    let options = SystemFlashOptions {
        katapult: SystemKatapultOptions {
            baud_rate: 250_000,
            bootloader_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(50),
            read_timeout: Duration::from_secs(5),
            can_bootloader_settle: Duration::from_millis(100),
        },
        host_mcu_unit_file: PathBuf::from(HOST_MCU_UNIT_FILE),
    };
    let moonraker = MoonrakerAdapter::new(&arguments.connection.moonraker.moonraker);
    let mut printer_checked = false;
    let mut accepted = Vec::new();
    for name in offered {
        let mcu = inventory
            .mcus
            .iter()
            .find(|mcu| mcu.name == name)
            .expect("offered MCU is discovered");
        let current = mcu.version.as_deref().unwrap_or("unknown");
        let next = checkout_label(&checkout);
        if arguments.auto {
            ui.heading(&format!("Update {name} from {current} to {next}"));
        } else {
            ui.prompt(&format!("Update {name} from {current} to {next}?"));
            if !crate::ui::confirmed() {
                continue;
            }
        }
        let pending = coordinator
            .prepare(&inventory, workspace, &name, arguments.clean)
            .inspect_err(|error| {
                tracing::debug!(?error, "flash preparation failed");
                ui.action(&format!("error: {}", aldis::error_chain(error)));
            })
            .with_context(|| format!("failed to prepare {name} for flashing"))?;
        if !printer_checked {
            ensure_printer_idle(&moonraker).map_err(|message| {
                ui.action(&format!("error: {message}"));
                anyhow::anyhow!(message)
            })?;
            printer_checked = true;
        }
        match coordinator.execute_and_flash_system_with_progress(
            pending.approve(),
            options.clone(),
            |progress| ui.progress(progress),
        ) {
            Ok(v) => {
                ui.finish_success(crate::ui::transfer_summary(
                    &mcu.kconfig,
                    v.flash.padded_bytes,
                ));
                ui.begin("waiting for MCU restart");
                if let Err(error) = wait_for_application(mcu) {
                    tracing::debug!(?error, "waiting for MCU restart failed");
                    ui.finish_failure();
                    ui.action(&format!("error: {error}"));
                    return Err(anyhow::anyhow!(error));
                }
                ui.finish_success("MCU restart confirmed");
                accepted.push(name);
            }
            Err(error) => {
                ui.finish_failure();
                tracing::debug!(?error, "flash failed");
                let restore = error
                    .allows_klipper_restore()
                    .then(|| coordinator.restore_after_failure());
                if let Some(Err(restore_error)) = &restore {
                    tracing::debug!(?restore_error, "restoring Klipper after the failure failed");
                }
                let message = update_failure(error, restore);
                ui.action(&format!("error: {message}"));
                return Err(anyhow::anyhow!(message));
            }
        }
    }
    if accepted.is_empty() {
        ui.block("\nno updates confirmed\n");
        ui.action("run completed successfully: no updates confirmed");
        return Ok(());
    }
    ui.heading("Finishing update");
    ui.begin("starting Klipper");
    if let Err(error) = coordinator.start_after_batch() {
        tracing::debug!(?error, "starting Klipper after batch failed");
        ui.finish_failure();
        ui.action(&format!("error: {}", aldis::error_chain(&error)));
        return Err(error.into());
    }
    ui.finish_success("Klipper ready");
    ui.begin("waiting for updated MCUs to reconnect");
    if let Err(error) = wait_for_mcus(&moonraker, &accepted, &checkout) {
        tracing::debug!(?error, "waiting for updated MCUs to reconnect failed");
        ui.finish_failure();
        ui.action(&format!("error: {error}"));
        return Err(anyhow::anyhow!(error));
    }
    ui.finish_success("all updated MCUs connected");
    ui.heading(&format!(
        "Update complete: {} {} updated (run log: {})",
        accepted.len(),
        mcu_count_label(accepted.len()),
        run_log_path.display()
    ));
    ui.action("run completed successfully");
    Ok(())
}

fn mcu_count_label(count: usize) -> &'static str {
    if count == 1 { "MCU" } else { "MCUs" }
}

fn update_failure(
    error: FlashCoordinatorError<SystemFlashError>,
    restore: Option<Result<(), aldis::coordinator::CoordinatorError>>,
) -> String {
    let detail = match error {
        FlashCoordinatorError::Coordinator(error) => aldis::error_chain(&error),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read the built firmware: {error}")
        }
        FlashCoordinatorError::Flash(error) => error.to_string(),
    };
    match restore {
        None => format!(
            "update failed: {detail}. Klipper may still be stopped; fix the issue, then restart it manually"
        ),
        Some(Ok(())) => format!(
            "update failed: {detail}. Klipper has been left in its state from before this update"
        ),
        Some(Err(restore_error)) => format!(
            "update failed: {detail}. Klipper also failed to restore: {}; fix the issue, then restart it manually",
            aldis::error_chain(&restore_error)
        ),
    }
}

pub(crate) fn ensure_printer_idle(printer: &impl PrinterStatePort) -> Result<(), String> {
    match printer.print_state() {
        Ok(state) if state.is_idle() => Ok(()),
        Ok(state) => Err(format!(
            "the printer is {state}; refusing to stop Klipper, run again when it is idle"
        )),
        Err(error) => Err(format!(
            "could not confirm the printer is idle; refusing to stop Klipper: {}",
            aldis::error_chain(&error)
        )),
    }
}

fn wait_for_application(mcu: &aldis::moonraker::Mcu) -> Result<(), String> {
    wait_for_application_with_timeout(mcu, Duration::from_secs(15))
}

fn wait_for_application_with_timeout(
    mcu: &aldis::moonraker::Mcu,
    timeout: Duration,
) -> Result<(), String> {
    let Some(McuTransport::Serial { device }) = &mcu.transport else {
        return Ok(());
    };
    retry_until_available(timeout, Duration::from_millis(100), || {
        std::path::Path::new(device)
            .exists()
            .then_some(())
            .ok_or(())
            .inspect_err(|()| {
                tracing::debug!(device = %device, "mcu device not yet re-enumerated, still waiting");
            })
    })
    .map_err(|()| format!("{} did not re-enumerate at {device}", mcu.name))
}

fn wait_for_mcus(
    client: &impl MoonrakerPort,
    selected: &[String],
    checkout: &CheckoutRevision,
) -> Result<(), String> {
    retry_until_available(Duration::from_secs(30), Duration::from_millis(250), || {
        let attempt_result = match client.discover_mcus() {
            Ok(inventory) => match pending_mcus(&inventory, selected, checkout) {
                pending if pending.is_empty() => Ok(()),
                pending => Err(format!("still waiting on: {}", pending.join(", "))),
            },
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

fn unknown_targets<'a>(inventory: &McuInventory, targets: &'a [String]) -> Vec<&'a str> {
    let mut unknown = Vec::new();
    for name in targets {
        if !inventory.mcus.iter().any(|mcu| mcu.name == *name) && !unknown.contains(&name.as_str())
        {
            unknown.push(name.as_str());
        }
    }
    unknown
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

fn selection_for(all: bool) -> UpdateSelection {
    if all {
        UpdateSelection::All
    } else {
        UpdateSelection::Required
    }
}

fn confirm_pull() -> bool {
    eprint!("Pull the configured Klipper upstream before updating? [Y/n] ");
    let _ = io::stderr().flush();
    crate::ui::confirmed()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ensure_printer_idle, mcu_count_label, pending_mcus, selection_for, unknown_targets,
        update_failure, wait_for_application_with_timeout, wait_for_mcus,
    };
    use aldis::moonraker::{PrintState, PrinterStatePort};

    struct FakePrinter(Result<PrintState, String>);

    impl PrinterStatePort for FakePrinter {
        fn print_state(&self) -> Result<PrintState, MoonrakerError> {
            self.0.clone().map_err(MoonrakerError::InvalidResponse)
        }
    }

    #[test]
    fn refuses_to_stop_klipper_while_a_print_is_running_or_paused() {
        for (state, label) in [
            (PrintState::Printing, "printing"),
            (PrintState::Paused, "paused"),
        ] {
            let message = ensure_printer_idle(&FakePrinter(Ok(state))).unwrap_err();

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
        let message =
            ensure_printer_idle(&FakePrinter(Ok(PrintState::Unknown("resuming".to_owned()))))
                .unwrap_err();

        assert!(
            message.starts_with("the printer is resuming; refusing"),
            "{message}"
        );
    }

    #[test]
    fn refuses_to_stop_klipper_when_the_print_state_cannot_be_read() {
        let message =
            ensure_printer_idle(&FakePrinter(Err("connection reset".to_owned()))).unwrap_err();

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
            assert!(ensure_printer_idle(&FakePrinter(Ok(state))).is_ok());
        }
    }
    use aldis::build::{BuildCommand, BuildError, CommandOutput};
    use aldis::coordinator::{CoordinatorError, FlashCoordinatorError};
    use aldis::eligibility::CheckoutRevision;
    use aldis::flash::system::SystemFlashError;
    use aldis::moonraker::{Mcu, McuInventory, McuTransport, MoonrakerError, MoonrakerPort};

    #[test]
    fn reports_mcu_counts_with_correct_pluralization() {
        assert_eq!(mcu_count_label(1), "MCU");
        assert_eq!(mcu_count_label(2), "MCUs");
    }

    #[test]
    fn selects_required_by_default_and_all_with_the_all_flag() {
        use aldis::eligibility::UpdateSelection;

        assert_eq!(selection_for(false), UpdateSelection::Required);
        assert_eq!(selection_for(true), UpdateSelection::All);
    }

    #[test]
    fn reports_the_host_mcu_setup_hint_in_the_final_update_error() {
        use aldis::flash::linux_host::{InstallStep, LinuxHostError};

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

        let message = update_failure(error, None);

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

        let message = update_failure(error, None);

        assert!(message.contains("make failed: permission denied"));
        assert!(!message.contains("unrelated output"));
        assert!(message.contains("may still be stopped"));
    }

    #[test]
    fn reports_that_klipper_was_restored_after_a_pre_flash_failure() {
        let error = FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other(
            "missing artifact",
        ));

        let message = update_failure(error, Some(Ok(())));

        assert!(message.contains("left in its state from before this update"));
        assert!(!message.contains("may still be stopped"));
    }

    #[test]
    fn reports_when_restoring_klipper_after_a_pre_flash_failure_also_fails() {
        let error = FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other(
            "missing artifact",
        ));
        let restore_error =
            CoordinatorError::UnexpectedKlipperState(aldis::service::ServiceState::Failed);

        let message = update_failure(error, Some(Err(restore_error)));

        assert!(message.contains("also failed to restore"));
        assert!(message.contains("restart it manually"));
    }

    #[test]
    fn rejects_a_serial_mcu_that_does_not_reenumerate() {
        let mcu = mcu("mcu h723", "v1", Some("/definitely/missing"));
        assert!(wait_for_application_with_timeout(&mcu, Duration::ZERO).is_err());
    }

    #[test]
    fn requires_every_selected_mcu_at_the_built_revision() {
        let selected = vec!["mcu h723".to_owned(), "mcu rp2040".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let only_one = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
        };
        assert_eq!(
            pending_mcus(&only_one, &selected, &checkout),
            vec!["mcu rp2040 (not reported by Moonraker)".to_owned()]
        );
        let wrong_version = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v1", None)],
        };
        assert_eq!(
            pending_mcus(&wrong_version, &selected, &checkout),
            vec!["mcu rp2040 (reports v1)".to_owned()]
        );
        let complete = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v2", None)],
        };
        assert!(pending_mcus(&complete, &selected, &checkout).is_empty());
    }

    #[test]
    fn wait_for_mcus_succeeds_once_the_injected_source_reports_readiness() {
        let selected = vec!["mcu h723".to_owned()];
        let checkout = CheckoutRevision::Known("v2".to_owned());
        let moonraker = FakeMoonraker(Ok(McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
        }));

        assert!(wait_for_mcus(&moonraker, &selected, &checkout).is_ok());
    }

    #[test]
    fn reports_every_target_name_with_no_matching_inventory_mcu() {
        let inventory = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
        };

        assert_eq!(
            unknown_targets(&inventory, &["mcu h723".to_owned()]),
            Vec::<&str>::new()
        );
        assert_eq!(
            unknown_targets(&inventory, &["mcu typo".to_owned()]),
            vec!["mcu typo"]
        );
        assert_eq!(
            unknown_targets(&inventory, &["mcu h723".to_owned(), "mcu typo".to_owned()]),
            vec!["mcu typo"]
        );
        assert_eq!(
            unknown_targets(&inventory, &["mcu typo".to_owned(), "mcu typo".to_owned()]),
            vec!["mcu typo"]
        );
    }

    struct FakeMoonraker(Result<McuInventory, String>);

    impl MoonrakerPort for FakeMoonraker {
        fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
            self.0.clone().map_err(MoonrakerError::InvalidResponse)
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
}
