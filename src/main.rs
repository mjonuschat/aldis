//! `aldis` CLI entry point: dispatches to the status, inspect, update, and setup commands.

mod cli;
mod setup;
mod status;
mod ui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use aldis::moonraker::{McuInventory, MoonrakerAdapter};
use anyhow::Context;
use clap::Parser;

use cli::{Cli, CliCommand, MoonrakerArgs};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Status(arguments) => status::status(arguments),
        CliCommand::Inspect(arguments) => match inspect(&arguments) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(format!("{error:#}")),
        },
        CliCommand::Update(arguments) => update::update(
            arguments,
            cli.verbose,
            ui::UpdateUi::new(cli.color, cli.no_progress),
        ),
        CliCommand::Setup(arguments) => setup::setup(arguments),
    }
}

fn inspect(arguments: &MoonrakerArgs) -> anyhow::Result<()> {
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
