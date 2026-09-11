use std::process::ExitCode;

use mcu_update::moonraker::{McuInventory, MoonrakerClient};
use mcu_update::plan::{UpdatePlan, build_update_plan};

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        print_usage();
        return ExitCode::from(2);
    };

    let moonraker_url = match parse_moonraker_arguments(&command, arguments.collect()) {
        Ok(url) => url,
        Err(message) => {
            eprintln!("error: {message}");
            print_usage();
            return ExitCode::from(2);
        }
    };
    let client = MoonrakerClient::new(&moonraker_url);
    match client.discover_mcus() {
        Ok(inventory) => {
            match command.as_str() {
                "inspect" => print_inventory(&moonraker_url, &inventory),
                "plan" => print_plan(&moonraker_url, &build_update_plan(&inventory)),
                _ => unreachable!("command validation happens while parsing arguments"),
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_moonraker_arguments(command: &str, arguments: Vec<String>) -> Result<String, String> {
    if !matches!(command, "inspect" | "plan") {
        return Err(format!("unknown command {command:?}"));
    }

    match arguments.as_slice() {
        [] => Ok(DEFAULT_MOONRAKER_URL.to_owned()),
        [flag, url] if flag == "--moonraker" => Ok(url.clone()),
        _ => Err("expected no arguments or --moonraker URL".to_owned()),
    }
}

fn print_plan(moonraker_url: &str, plan: &UpdatePlan) {
    println!("phase: plan");
    println!("Moonraker: {moonraker_url}");
    println!("MCUs: {}", plan.targets.len());
    println!("\nKlipper lifecycle:");
    println!(
        "  discovery: {}",
        if plan.klipper.runs_during_discovery {
            "running"
        } else {
            "not running"
        }
    );
    println!(
        "  stop: {}",
        if plan.klipper.stops_before_first_build_or_flash {
            "immediately before the first selected build or flash"
        } else {
            "not required by this plan"
        }
    );
    println!(
        "  restart: {}",
        if plan.klipper.restarts_automatically {
            "automatic"
        } else {
            "explicit operator action after the run"
        }
    );

    for target in &plan.targets {
        println!("\n{} ({})", target.name, target.mcu);
        for step in &target.steps {
            println!("  - {}", step.label());
        }
    }
}

fn print_inventory(moonraker_url: &str, inventory: &McuInventory) {
    println!("phase: discover");
    println!("Moonraker: {moonraker_url}");
    println!("MCUs: {}", inventory.mcus.len());

    for mcu in &inventory.mcus {
        println!("\n{}", mcu.name);
        println!("  chip: {}", mcu.mcu);
        if let Some(app) = &mcu.app {
            println!("  application: {app}");
        }
        if let Some(version) = &mcu.version {
            println!("  version: {version}");
        }
        if let Some(frequency) = mcu.canbus_frequency_hz {
            println!("  CAN: {frequency} Hz");
        }
        println!("  Kconfig settings: {}", mcu.kconfig.lines().count());
    }
}

fn print_usage() {
    eprintln!("usage: mcu-update <inspect|plan> [--moonraker URL]");
}
