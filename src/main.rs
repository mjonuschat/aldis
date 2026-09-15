//! `mcu-update` CLI entry point: dispatches to the status, inspect, update, and setup commands.

mod cli;
mod setup;
mod status;
mod ui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use mcu_update::moonraker::{McuInventory, MoonrakerAdapter};

use cli::{Cli, CliCommand};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Status(arguments) => status::status(arguments),
        CliCommand::Inspect(arguments) => {
            match MoonrakerAdapter::new(&arguments.moonraker).discover_mcus() {
                Ok(inventory) => {
                    print_inventory(&arguments.moonraker, &inventory);
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error.to_string()),
            }
        }
        CliCommand::Update(arguments) => {
            update::update(arguments, ui::UpdateUi::new(cli.color, cli.no_progress))
        }
        CliCommand::Setup(arguments) => setup::setup(arguments),
    }
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
