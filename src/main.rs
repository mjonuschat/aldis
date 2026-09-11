use std::process::ExitCode;

use mcu_update::moonraker::{McuInventory, MoonrakerClient};

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        print_usage();
        return ExitCode::from(2);
    };

    if command != "inspect" {
        print_usage();
        return ExitCode::from(2);
    }

    let moonraker_url = match parse_inspect_arguments(arguments.collect()) {
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
            print_inventory(&moonraker_url, &inventory);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_inspect_arguments(arguments: Vec<String>) -> Result<String, String> {
    match arguments.as_slice() {
        [] => Ok(DEFAULT_MOONRAKER_URL.to_owned()),
        [flag, url] if flag == "--moonraker" => Ok(url.clone()),
        _ => Err("expected no arguments or --moonraker URL".to_owned()),
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
    eprintln!("usage: mcu-update inspect [--moonraker URL]");
}
