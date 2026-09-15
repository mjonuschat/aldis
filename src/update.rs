//! The interactive build-and-flash update command.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcu_update::build::SystemCommandRunner;
use mcu_update::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use mcu_update::coordinator::{BuildCoordinator, FlashCoordinatorError};
use mcu_update::eligibility::{
    CheckoutRevision, Eligibility, UpdateSelection, assess_mcu, is_selected,
};
use mcu_update::flash::katapult::system::SystemKatapultOptions;
use mcu_update::flash::system::{SystemFlashError, SystemFlashOptions};
use mcu_update::moonraker::{McuInventory, McuTransport, MoonrakerClient, MoonrakerPort};
use mcu_update::plan::build_update_plan;
use mcu_update::retry::retry_until_available;
use mcu_update::run_log::{LoggingCommandRunner, RunLog};
use mcu_update::workspace::RunWorkspace;

use crate::cli::UpdateArgs;
use crate::fail;
use crate::status::{checkout_label, format_status, refresh_label};
use crate::ui::UpdateUi;

pub(crate) fn update(arguments: UpdateArgs, mut ui: UpdateUi) -> ExitCode {
    let source = arguments
        .connection
        .klipper_source
        .unwrap_or_else(crate::default_klipper_source);
    let workspace_path = arguments.workspace.unwrap_or_else(default_run_workspace);
    let workspace = match RunWorkspace::create(workspace_path) {
        Ok(workspace) => workspace,
        Err(error) => return fail(error.to_string()),
    };
    let run_log = match RunLog::create(workspace.root()) {
        Ok(log) => log,
        Err(error) => return fail(error.to_string()),
    };
    ui.set_log(run_log.clone());
    ui.action(&format!(
        "updating from Klipper source {}",
        source.display()
    ));
    let refreshed =
        if arguments.auto || arguments.pull || (io::stdin().is_terminal() && confirm_pull()) {
            ui.action("refreshing configured Klipper upstream");
            let refreshed = match refresh_checkout(&source) {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    ui.action(&format!("error: {error}"));
                    return fail(error.to_string());
                }
            };
            ui.action(&format!("checkout refresh: {}", refresh_label(&refreshed)));
            Some(refreshed)
        } else {
            None
        };
    ui.action("discovering MCUs from Moonraker");
    let inventory =
        match MoonrakerClient::new(&arguments.connection.moonraker.moonraker).discover_mcus() {
            Ok(v) => v,
            Err(error) => {
                ui.action(&format!("error: {error}"));
                return fail(error.to_string());
            }
        };
    let plan = build_update_plan(&inventory);
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    ui.block(&format_status(
        &source,
        &checkout,
        &inventory,
        refreshed.as_ref(),
    ));
    ui.block(&format!(
        "Run log:           {}\n",
        run_log.path().display()
    ));
    let selection = if arguments.all {
        UpdateSelection::All
    } else {
        UpdateSelection::Required
    };
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
        return ExitCode::SUCCESS;
    }
    let coordinator = BuildCoordinator::new(
        &source,
        LoggingCommandRunner::new(SystemCommandRunner, run_log.clone()),
        LoggingCommandRunner::new(SystemCommandRunner, run_log.clone()),
    );
    let options = SystemFlashOptions {
        katapult: SystemKatapultOptions {
            baud_rate: 250_000,
            bootloader_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(50),
            read_timeout: Duration::from_millis(100),
            can_bootloader_settle: Duration::from_millis(100),
        },
        bossac_program: source.join("lib/bossac/bin/bossac"),
    };
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
            if !matches!(read_confirmation().as_deref(), Ok("y") | Ok("yes")) {
                continue;
            }
        }
        let pending = match coordinator.prepare(&inventory, &plan, &workspace, &name) {
            Ok(v) => v,
            Err(error) => {
                ui.action(&format!("error: {error}"));
                return fail(error.to_string());
            }
        };
        match coordinator.execute_and_flash_system_with_progress_and_log(
            pending.approve(),
            options.clone(),
            Some(&run_log),
            |progress| ui.progress(progress),
        ) {
            Ok(v) => {
                ui.finish_success(format!("flashed {} bytes", v.flash.padded_bytes));
                ui.begin("waiting for MCU restart");
                if let Err(error) = wait_for_application(mcu) {
                    ui.finish_failure();
                    ui.action(&format!("error: {error}"));
                    return fail(error);
                }
                ui.finish_success("MCU restart confirmed");
                accepted.push(name);
            }
            Err(error) => {
                ui.finish_failure();
                let message = update_failure(error);
                ui.action(&format!("error: {message}"));
                return fail(message);
            }
        }
    }
    if accepted.is_empty() {
        ui.block("\nno updates confirmed\n");
        ui.action("run completed successfully: no updates confirmed");
        return ExitCode::SUCCESS;
    }
    ui.heading("Finishing update");
    ui.begin("starting Klipper");
    if let Err(error) = coordinator.start_after_batch() {
        ui.finish_failure();
        let message = error.to_string();
        ui.action(&format!("error: {message}"));
        return fail(message);
    }
    ui.finish_success("Klipper ready");
    ui.begin("waiting for updated MCUs to reconnect");
    if let Err(error) = wait_for_mcus(
        &MoonrakerClient::new(&arguments.connection.moonraker.moonraker),
        &accepted,
        &checkout,
    ) {
        ui.finish_failure();
        ui.action(&format!("error: {error}"));
        return fail(error);
    }
    ui.finish_success("all updated MCUs connected");
    ui.heading(&format!(
        "Update complete: {} {} updated",
        accepted.len(),
        mcu_count_label(accepted.len())
    ));
    ui.action("run completed successfully");
    ExitCode::SUCCESS
}

fn default_run_workspace() -> PathBuf {
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    state_dir
        .join("mcu-update/runs")
        .join(format!("run-{nonce}-{}", std::process::id()))
}

fn mcu_count_label(count: usize) -> &'static str {
    if count == 1 { "MCU" } else { "MCUs" }
}

fn update_failure(error: FlashCoordinatorError<SystemFlashError>) -> String {
    let detail = match error {
        FlashCoordinatorError::Coordinator(error) => error.to_string(),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read the built firmware: {error}")
        }
        FlashCoordinatorError::Flash(error) => error.to_string(),
    };
    format!(
        "update failed: {detail}. Klipper may still be stopped; fix the issue, then rerun update"
    )
}

fn wait_for_application(mcu: &mcu_update::moonraker::Mcu) -> Result<(), String> {
    wait_for_application_with_timeout(mcu, Duration::from_secs(15))
}

fn wait_for_application_with_timeout(
    mcu: &mcu_update::moonraker::Mcu,
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
    })
    .map_err(|()| format!("{} did not re-enumerate at {device}", mcu.name))
}

fn wait_for_mcus(
    client: &impl MoonrakerPort,
    selected: &[String],
    checkout: &CheckoutRevision,
) -> Result<(), String> {
    retry_until_available(Duration::from_secs(30), Duration::from_millis(250), || {
        client
            .discover_mcus()
            .ok()
            .filter(|inventory| selected_mcus_are_ready(inventory, selected, checkout))
            .map(|_| ())
            .ok_or(())
    })
    .map_err(|()| "Klipper did not reconnect every updated MCU at the built revision".to_owned())
}

fn selected_mcus_are_ready(
    inventory: &McuInventory,
    selected: &[String],
    checkout: &CheckoutRevision,
) -> bool {
    selected.iter().all(|name| {
        inventory
            .mcus
            .iter()
            .find(|mcu| &mcu.name == name)
            .is_some_and(|mcu| match checkout {
                CheckoutRevision::Known(revision) => mcu.version.as_deref() == Some(revision),
                CheckoutRevision::Indeterminate => true,
            })
    })
}

fn confirm_pull() -> bool {
    eprint!("Pull the configured Klipper upstream before updating? [Y/n] ");
    let _ = io::stderr().flush();
    matches!(read_confirmation().as_deref(), Ok("") | Ok("y") | Ok("yes"))
}

fn read_confirmation() -> Result<String, io::Error> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        mcu_count_label, selected_mcus_are_ready, update_failure,
        wait_for_application_with_timeout, wait_for_mcus,
    };
    use mcu_update::build::{BuildCommand, BuildError, CommandOutput};
    use mcu_update::coordinator::{CoordinatorError, FlashCoordinatorError};
    use mcu_update::eligibility::CheckoutRevision;
    use mcu_update::flash::system::SystemFlashError;
    use mcu_update::moonraker::{Mcu, McuInventory, McuTransport, MoonrakerError, MoonrakerPort};

    #[test]
    fn reports_mcu_counts_with_correct_pluralization() {
        assert_eq!(mcu_count_label(1), "MCU");
        assert_eq!(mcu_count_label(2), "MCUs");
    }

    #[test]
    fn reports_build_stderr_without_debugging_command_buffers() {
        let error = FlashCoordinatorError::<SystemFlashError>::Coordinator(
            CoordinatorError::Build(BuildError::CommandFailed {
                command: Box::new(BuildCommand {
                    program: "make".to_owned(),
                    arguments: Vec::new(),
                    current_dir: None,
                }),
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: b"unrelated output".to_vec(),
                    stderr: b"permission denied".to_vec(),
                }),
            }),
        );

        let message = update_failure(error);

        assert!(message.contains("make failed: permission denied"));
        assert!(!message.contains("unrelated output"));
        assert!(message.contains("may still be stopped"));
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
        assert!(!selected_mcus_are_ready(&only_one, &selected, &checkout));
        let wrong_version = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v1", None)],
        };
        assert!(!selected_mcus_are_ready(
            &wrong_version,
            &selected,
            &checkout
        ));
        let complete = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None), mcu("mcu rp2040", "v2", None)],
        };
        assert!(selected_mcus_are_ready(&complete, &selected, &checkout));
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
