//! The interactive build-and-flash update command.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use aldis::config::{FleetConfig, instance_urls};
use aldis::coordinator::{BuildCoordinator, UpdateProgress};
use aldis::eligibility::{
    CheckoutRevision, TargetVersions, UpdateSelection, classify_mcu, is_selected,
    removes_calibration,
};
use aldis::fleet::peer::{check_bridge, peers_excluding};
use aldis::fleet::system::{other_urls, probe_peer};
use aldis::logging::{self, LoggingCommandAdapter};
use aldis::moonraker::{McuInventory, MoonrakerAdapter};
use aldis::service::KlipperUnit;
use aldis::update_run::{RunHooks, RunRequest, RunStep};
use aldis::workspace::RunWorkspace;

use crate::cli::UpdateArgs;
use crate::fail;
use crate::status::{format_status, refresh_label};
use crate::ui::UpdateUi;

/// The other printers named by `config`, or none without one.
pub(crate) fn peer_urls(config: Option<&Path>, own_url: &str) -> anyhow::Result<Vec<String>> {
    let Some(path) = config else {
        return Ok(Vec::new());
    };
    let file = FleetConfig::load(path)?;
    Ok(other_urls(own_url, &instance_urls(Some(file), &[])))
}

/// Refuses a USB-CAN bridge flash while another printer named by `config` is busy or cannot be
/// checked. Does nothing without a config.
pub(crate) fn bridge_refusal(
    config: Option<&Path>,
    own_url: &str,
    own_unit: &KlipperUnit,
    inventory: &McuInventory,
    targets: &[String],
) -> anyhow::Result<()> {
    let urls = peer_urls(config, own_url)?;
    if urls.is_empty() {
        return Ok(());
    }
    let peers = urls.iter().map(|url| probe_peer(url)).collect();
    let peers = peers_excluding(own_unit.as_str(), peers);
    check_bridge(inventory, targets, &peers)
        .map_err(|message| anyhow::anyhow!("refusing to flash: {message}"))
}

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
        if arguments.auto || arguments.pull || (io::stdin().is_terminal() && confirm_pull(ui)) {
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
    let moonraker = MoonrakerAdapter::new(moonraker_url);
    let unit = aldis::host::verify_host(moonraker_url, &moonraker)
        .context("refusing to update firmware")?;
    ui.action("discovering MCUs from Moonraker");
    let mut inventory = crate::discovery::discover_mcus_with_retry(&moonraker)
        .inspect_err(|error| {
            tracing::debug!(?error, "Moonraker discovery failed");
            ui.action(&format!("error: {}", aldis::error_chain(error)));
        })
        .context("failed to discover MCUs from Moonraker")?;
    crate::discovery::probe_unreported(moonraker_url, &moonraker, &mut inventory);
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
    let releases = aldis::foci::GithubReleases::new();
    let firmware_sources = arguments.connection.sources.sources();
    let versions = TargetVersions::klipper(
        checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate),
    )
    .with_prebuilt(crate::discovery::prebuilt_targets(
        &inventory,
        &moonraker,
        &releases,
        &firmware_sources,
        &source,
    ));
    ui.block(&format_status(
        &source,
        &versions,
        &inventory,
        refreshed.as_ref(),
    ));
    // `--force` offers every eligible MCU among the named ones (or all of them), as `--all` does.
    let selection = selection_for(arguments.all || arguments.force);
    for mcu in inventory
        .mcus
        .iter()
        .filter(|mcu| arguments.targets.contains(&mcu.name))
    {
        if let aldis::eligibility::Eligibility::Prebuilt(running) = classify_mcu(mcu)
            && let Some(refusal) = versions.prebuilt.downgrade_refusal(&running, &mcu.name)
        {
            ui.block(&format!("\n{refusal}\n"));
        }
    }
    let offered = inventory
        .mcus
        .iter()
        .filter(|mcu| arguments.targets.is_empty() || arguments.targets.contains(&mcu.name))
        .filter(|mcu| is_selected(mcu, &versions, &selection))
        .map(|mcu| mcu.name.clone())
        .collect::<Vec<_>>();
    let offered = aldis::flash_order::flash_order(&inventory, &offered);
    if offered.is_empty() {
        ui.block("\nno eligible MCUs require an update\n");
        ui.action("run completed successfully: no eligible MCUs require an update");
        return Ok(());
    }
    bridge_refusal(
        arguments.config.as_deref(),
        moonraker_url,
        &unit,
        &inventory,
        &offered,
    )?;
    let coordinator = BuildCoordinator::with_unit(
        &source,
        LoggingCommandAdapter::new(SystemCommandAdapter),
        LoggingCommandAdapter::new(SystemCommandAdapter),
        unit,
    );
    let recalibrate: Vec<String> = inventory
        .mcus
        .iter()
        .filter(|mcu| offered.contains(&mcu.name) && removes_calibration(mcu))
        .map(|mcu| mcu.name.clone())
        .collect();
    let mut hooks = CliHooks {
        ui,
        auto: arguments.auto,
        recalibrate: &recalibrate,
    };
    let outcome = aldis::update_run::run_updates(
        &coordinator,
        &moonraker,
        &inventory,
        workspace,
        RunRequest {
            targets: &offered,
            target_versions: &versions,
            foci_releases: &releases,
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
    for name in outcome
        .updated
        .iter()
        .filter(|name| recalibrate.contains(name))
    {
        ui.block(&format!(
            "\nRecalibrate {name} before printing: Cartographer's plugin removed the scan and \
             touch models made with its old firmware.\n"
        ));
    }
    ui.action("run completed successfully");
    Ok(())
}

/// The approval question for `name`, which for an MCU in `recalibrate` says what updating it
/// costs.
fn approval_question(name: &str, current: &str, next: &str, recalibrate: &[String]) -> String {
    let question = format!("Update {name} from {current} to {next}?");
    if recalibrate.iter().any(|candidate| candidate == name) {
        format!(
            "{question} Cartographer's plugin then removes its scan and touch models, so it must \
             be recalibrated before printing."
        )
    } else {
        question
    }
}

struct CliHooks<'a> {
    ui: &'a mut UpdateUi,
    auto: bool,
    /// MCUs whose update removes their calibration.
    recalibrate: &'a [String],
}

impl RunHooks for CliHooks<'_> {
    fn approve(&mut self, name: &str, current: &str, next: &str) -> bool {
        if self.auto {
            self.ui
                .heading(&format!("Update {name} from {current} to {next}"));
            true
        } else {
            self.ui
                .confirm(&approval_question(name, current, next, self.recalibrate))
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

fn selection_for(every_eligible: bool) -> UpdateSelection {
    if every_eligible {
        UpdateSelection::All
    } else {
        UpdateSelection::Required
    }
}

fn confirm_pull(ui: &mut UpdateUi) -> bool {
    ui.confirm("Pull the configured Klipper upstream before updating?")
}

#[cfg(test)]
mod tests {
    use super::{approval_question, mcu_count_label, selection_for, unknown_targets};

    #[test]
    fn warns_in_the_approval_question_when_an_update_removes_calibration() {
        let recalibrate = ["mcu cartographer".to_owned()];

        assert_eq!(
            approval_question("mcu", "v1", "v2", &recalibrate),
            "Update mcu from v1 to v2?"
        );
        let question = approval_question(
            "mcu cartographer",
            "CARTOGRAPHER V4 6.0.0",
            "CARTOGRAPHER V4 6.1.0",
            &recalibrate,
        );
        assert!(
            question.starts_with(
                "Update mcu cartographer from CARTOGRAPHER V4 6.0.0 to CARTOGRAPHER V4 6.1.0? "
            ),
            "{question}"
        );
        assert!(question.contains("must be recalibrated"), "{question}");
    }

    #[test]
    fn words_the_calibration_prompt_exactly() {
        assert_eq!(
            approval_question(
                "mcu cartographer",
                "CARTOGRAPHER V4 6.0.0",
                "CARTOGRAPHER V4 6.1.0",
                &["mcu cartographer".to_owned()]
            ),
            "Update mcu cartographer from CARTOGRAPHER V4 6.0.0 to CARTOGRAPHER V4 6.1.0? \
             Cartographer's plugin then removes its scan and touch models, so it must be \
             recalibrated before printing."
        );
    }

    #[test]
    fn without_a_config_there_are_no_peers_and_nothing_to_check() {
        assert_eq!(
            super::peer_urls(None, "http://127.0.0.1:7125").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_config_supplies_every_other_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aldis.toml");
        std::fs::write(
            &path,
            r#"moonraker = ["http://127.0.0.1:7125", "http://127.0.0.1:7126/"]"#,
        )
        .unwrap();

        assert_eq!(
            super::peer_urls(Some(path.as_path()), "http://127.0.0.1:7125").unwrap(),
            ["http://127.0.0.1:7126"]
        );
        let missing = dir.path().join("missing.toml");
        assert!(super::peer_urls(Some(missing.as_path()), "http://127.0.0.1:7125").is_err());
    }

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
            build_versions: None,
            transport: serial.map(|device| aldis::moonraker::McuTransport::Serial {
                device: device.to_owned(),
            }),
            kconfig: "CONFIG_TEST=y\n".to_owned(),
        }
    }
}
