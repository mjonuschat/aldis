//! `aldis` CLI entry point: dispatches to the status, inspect, update, and setup commands.

mod cli;
mod setup;
mod status;
mod ui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use aldis::logging;
use aldis::moonraker::{McuInventory, MoonrakerAdapter};
use anyhow::Context;
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;

use cli::{Cli, CliCommand, MoonrakerArgs};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Status(arguments) => {
            let _guard = init_logging(cli.verbose);
            status::status(arguments)
        }
        CliCommand::Inspect(arguments) => {
            let _guard = init_logging(cli.verbose);
            match inspect(&arguments) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(format!("{error:#}")),
            }
        }
        // `update` owns its logging::init call (routed to the run workspace's
        // run.log) via update.rs; do not init here too, or the second
        // try_init() call would fail.
        CliCommand::Update(arguments) => update::update(
            arguments,
            cli.verbose,
            ui::UpdateUi::new(cli.color, cli.no_progress),
        ),
        CliCommand::Setup(arguments) => {
            let _guard = init_logging(cli.verbose);
            setup::setup(arguments)
        }
    }
}

/// `status`/`inspect`/`setup` have no run workspace to anchor a log file to,
/// so each process gets its own path in the system temp directory, keyed by
/// pid so different users/invocations never collide on the same file.
///
/// File logging is best-effort: if the log file cannot be opened (e.g. a
/// previous run left behind a file owned by another user), this warns on
/// stderr and returns `None` rather than aborting the command — the
/// subcommand's actual work must still run.
fn init_logging(verbose: u8) -> Option<WorkerGuard> {
    let log_path = std::env::temp_dir().join(format!("aldis-{}.log", std::process::id()));
    match logging::init(verbose, &log_path) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("warning: could not start file logging: {error:#}");
            if let Err(error) = logging::init_stderr_only(verbose) {
                eprintln!("warning: could not start stderr logging: {error:#}");
            }
            None
        }
    }
}

fn inspect(arguments: &MoonrakerArgs) -> anyhow::Result<()> {
    tracing::info!("discovering MCUs from Moonraker");
    let inventory = MoonrakerAdapter::new(&arguments.moonraker)
        .discover_mcus()
        .context("failed to discover MCUs")?;
    print_inventory(&arguments.moonraker, &inventory);
    Ok(())
}

pub(crate) fn default_klipper_source() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("klipper"))
        .unwrap_or_else(|| PathBuf::from("klipper"))
}

pub(crate) fn fail(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}

fn print_inventory(url: &str, inventory: &McuInventory) {
    println!(
        "phase: discover\nMoonraker: {url}\nMCUs: {}",
        inventory.mcus.len()
    );
    for mcu in &inventory.mcus {
        println!(
            "\n{}\n  chip: {}\n  Kconfig settings: {}",
            mcu.name,
            mcu.mcu,
            mcu.kconfig.lines().count()
        );
    }
}
