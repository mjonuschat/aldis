//! The interactive build-and-flash update command.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use aldis::coordinator::{BuildCoordinator, UpdateProgress};
use aldis::eligibility::{CheckoutRevision, Eligibility, UpdateSelection, assess_mcu, is_selected};
use aldis::logging::{self, LoggingCommandAdapter};
use aldis::moonraker::{McuInventory, MoonrakerAdapter};
use aldis::update_run::{RunHooks, RunRequest, RunStep};
use aldis::workspace::RunWorkspace;

use crate::cli::UpdateArgs;
use crate::fail;
use crate::status::{format_status, refresh_label};
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
    let _lock =
        aldis::lock::UpdateLock::try_acquire_default().context("could not start the update")?;
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
    let moonraker_url = &arguments.connection.moonraker.moonraker;
    aldis::host::verify_host(moonraker_url, &MoonrakerAdapter::new(moonraker_url))
        .context("refusing to update firmware")?;
    ui.action("discovering MCUs from Moonraker");
    let mut inventory = crate::discovery::discover_mcus_with_retry(&MoonrakerAdapter::new(
        &arguments.connection.moonraker.moonraker,
    ))
    .inspect_err(|error| {
        tracing::debug!(?error, "Moonraker discovery failed");
        ui.action(&format!("error: {}", aldis::error_chain(error)));
    })
    .context("failed to discover MCUs from Moonraker")?;
    crate::discovery::probe_unreported(
        moonraker_url,
        &MoonrakerAdapter::new(moonraker_url),
        &mut inventory,
    );
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
    let offered = aldis::flash_order::flash_order(&inventory, &offered);
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
    let moonraker = MoonrakerAdapter::new(moonraker_url);
    let mut hooks = CliHooks {
        ui,
        auto: arguments.auto,
    };
    let outcome = aldis::update_run::run_updates(
        &coordinator,
        &moonraker,
        &inventory,
        workspace,
        RunRequest {
            targets: &offered,
            target_revision: &checkout,
            clean: arguments.clean,
        },
        &mut hooks,
    )
    .map_err(|failure| anyhow::anyhow!(failure.message))?;
    let ui = hooks.ui;
    if outcome.updated.is_empty() {
        ui.block("\nno updates confirmed\n");
        ui.action("run completed successfully: no updates confirmed");
        return Ok(());
    }
    ui.heading(&format!(
        "Update complete: {} {} updated (run log: {})",
        outcome.updated.len(),
        mcu_count_label(outcome.updated.len()),
        run_log_path.display()
    ));
    ui.action("run completed successfully");
    Ok(())
}

struct CliHooks<'a> {
    ui: &'a mut UpdateUi,
    auto: bool,
}

impl RunHooks for CliHooks<'_> {
    fn approve(&mut self, name: &str, current: &str, next: &str) -> bool {
        if self.auto {
            self.ui
                .heading(&format!("Update {name} from {current} to {next}"));
            true
        } else {
            self.ui
                .prompt(&format!("Update {name} from {current} to {next}?"));
            crate::ui::confirmed()
        }
    }

    fn progress(&mut self, _: &str, progress: UpdateProgress) {
        self.ui.progress(progress);
    }

    fn flashed(&mut self, _: &str, kconfig: &str, padded_bytes: usize) {
        self.ui
            .finish_success(crate::ui::transfer_summary(kconfig, padded_bytes));
    }

    fn step_started(&mut self, step: RunStep) {
        match step {
            RunStep::WaitForApplication => self.ui.begin("waiting for MCU restart"),
            RunStep::StartKlipper => {
                self.ui.heading("Finishing update");
                self.ui.begin("starting Klipper");
            }
            RunStep::Reconnect => self.ui.begin("waiting for updated MCUs to reconnect"),
        }
    }

    fn step_succeeded(&mut self, step: RunStep) {
        self.ui.finish_success(match step {
            RunStep::WaitForApplication => "MCU restart confirmed",
            RunStep::StartKlipper => "Klipper ready",
            RunStep::Reconnect => "all updated MCUs connected",
        });
    }

    fn failed(&mut self, message: &str) {
        self.ui.finish_failure();
        self.ui.action(&format!("error: {message}"));
    }
}

fn mcu_count_label(count: usize) -> &'static str {
    if count == 1 { "MCU" } else { "MCUs" }
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
    use super::{mcu_count_label, selection_for, unknown_targets};
    use aldis::moonraker::{Mcu, McuInventory};

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
    fn reports_every_target_name_with_no_matching_inventory_mcu() {
        let inventory = McuInventory {
            mcus: vec![mcu("mcu h723", "v2", None)],
            unreported: Vec::new(),
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

    fn mcu(name: &str, version: &str, serial: Option<&str>) -> Mcu {
        Mcu {
            name: name.to_owned(),
            app: None,
            version: Some(version.to_owned()),
            mcu: "test".to_owned(),
            canbus_frequency_hz: None,
            transport: serial.map(|device| aldis::moonraker::McuTransport::Serial {
                device: device.to_owned(),
            }),
            kconfig: "CONFIG_TEST=y\n".to_owned(),
        }
    }
}
