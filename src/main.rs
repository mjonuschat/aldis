use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use mcu_update::build::SystemCommandRunner;
use mcu_update::coordinator::BuildCoordinator;
use mcu_update::flash::katapult::system::SystemKatapultOptions;
use mcu_update::moonraker::{McuInventory, MoonrakerClient};
use mcu_update::plan::{UpdatePlan, build_update_plan};
use mcu_update::workspace::RunWorkspace;

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return usage("missing command");
    };
    match command.as_str() {
        "inspect" | "plan" => {
            let url = match moonraker(args.collect()) {
                Ok(url) => url,
                Err(e) => return usage(&e),
            };
            match MoonrakerClient::new(&url).discover_mcus() {
                Ok(inventory) if command == "inspect" => {
                    print_inventory(&url, &inventory);
                    ExitCode::SUCCESS
                }
                Ok(inventory) => {
                    print_plan(&url, &build_update_plan(&inventory));
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error.to_string()),
            }
        }
        "update" => update(args.collect()),
        _ => usage("unknown command"),
    }
}

fn update(args: Vec<String>) -> ExitCode {
    let Some((target, options)) = args.split_first() else {
        return usage("update requires an MCU target");
    };
    let mut url = DEFAULT_MOONRAKER_URL.to_owned();
    let mut source = None;
    let mut workspace = None;
    let mut yes = false;
    let mut it = options.iter();
    while let Some(option) = it.next() {
        match option.as_str() {
            "--yes" => yes = true,
            "--moonraker" => match it.next() {
                Some(v) => url = v.clone(),
                None => return usage("--moonraker requires a URL"),
            },
            "--klipper-source" => match it.next() {
                Some(v) => source = Some(PathBuf::from(v)),
                None => return usage("--klipper-source requires a path"),
            },
            "--workspace" => match it.next() {
                Some(v) => workspace = Some(PathBuf::from(v)),
                None => return usage("--workspace requires a path"),
            },
            _ => return usage("unknown update option"),
        }
    }
    let (Some(source), Some(workspace)) = (source, workspace) else {
        return usage("update requires --klipper-source PATH and --workspace PATH");
    };
    let inventory = match MoonrakerClient::new(&url).discover_mcus() {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let plan = build_update_plan(&inventory);
    let Some(selected) = plan.targets.iter().find(|v| v.name == *target) else {
        return fail(format!("MCU target {target:?} is not in the update plan"));
    };
    println!(
        "target: {} ({})\ntransport: {:?}\nworkspace: {}",
        selected.name,
        selected.mcu,
        selected.transport,
        workspace.display()
    );
    if !yes {
        return usage("confirmation required: rerun with --yes to stop Klipper, build, and flash");
    }
    let workspace = match RunWorkspace::create(workspace) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let coordinator = BuildCoordinator::new(source, SystemCommandRunner, SystemCommandRunner);
    let pending = match coordinator.prepare(&inventory, &plan, &workspace, target) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let options = SystemKatapultOptions {
        baud_rate: 250_000,
        bootloader_timeout: Duration::from_secs(10),
        poll_interval: Duration::from_millis(50),
        read_timeout: Duration::from_millis(100),
        can_bootloader_settle: Duration::from_millis(100),
    };
    match coordinator.execute_and_flash_system(pending.approve(), options) {
        Ok(v) => {
            println!(
                "flashed {} bytes from {}",
                v.flash.padded_bytes,
                v.artifact.path.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("update failed: {e:?}")),
    }
}

fn moonraker(args: Vec<String>) -> Result<String, String> {
    match args.as_slice() {
        [] => Ok(DEFAULT_MOONRAKER_URL.to_owned()),
        [flag, url] if flag == "--moonraker" => Ok(url.clone()),
        _ => Err("expected no arguments or --moonraker URL".to_owned()),
    }
}
fn fail(message: String) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}
fn usage(message: &str) -> ExitCode {
    eprintln!(
        "error: {message}\nusage: mcu-update <inspect|plan> [--moonraker URL]\n       mcu-update update <target> --klipper-source PATH --workspace NEW_PATH --yes [--moonraker URL]"
    );
    ExitCode::from(2)
}
fn print_plan(url: &str, plan: &UpdatePlan) {
    println!(
        "phase: plan\nMoonraker: {url}\nMCUs: {}",
        plan.targets.len()
    );
    for target in &plan.targets {
        println!("\n{} ({})", target.name, target.mcu);
        for step in &target.steps {
            println!("  - {}", step.label());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::moonraker;

    #[test]
    fn accepts_the_default_or_explicit_moonraker_url() {
        assert_eq!(moonraker(Vec::new()).unwrap(), "http://127.0.0.1:7125");
        assert_eq!(
            moonraker(vec![
                "--moonraker".to_owned(),
                "http://example.test".to_owned()
            ])
            .unwrap(),
            "http://example.test"
        );
    }

    #[test]
    fn rejects_invalid_read_only_arguments() {
        assert!(moonraker(vec!["--moonraker".to_owned()]).is_err());
    }
}
