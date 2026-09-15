use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use mcu_update::build::SystemCommandRunner;
use mcu_update::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use mcu_update::coordinator::BuildCoordinator;
use mcu_update::eligibility::{CheckoutRevision, UpdateSelection, assess_mcu, is_selected};
use mcu_update::flash::katapult::system::SystemKatapultOptions;
use mcu_update::flash::system::SystemFlashOptions;
use mcu_update::moonraker::{McuInventory, McuTransport, MoonrakerClient};
use mcu_update::plan::build_update_plan;
use mcu_update::workspace::RunWorkspace;

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return usage("missing command");
    };
    match command.as_str() {
        "status" => status(args.collect()),
        "inspect" => {
            let url = match moonraker(args.collect()) {
                Ok(url) => url,
                Err(e) => return usage(&e),
            };
            match MoonrakerClient::new(&url).discover_mcus() {
                Ok(inventory) => {
                    print_inventory(&url, &inventory);
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error.to_string()),
            }
        }
        "update" => update(args.collect()),
        _ => usage("unknown command"),
    }
}

fn status(arguments: Vec<String>) -> ExitCode {
    let (url, source) = match status_arguments(arguments) {
        Ok(values) => values,
        Err(error) => return usage(&error),
    };
    let inventory = match MoonrakerClient::new(&url).discover_mcus() {
        Ok(inventory) => inventory,
        Err(error) => return fail(error.to_string()),
    };
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    println!(
        "Klipper source: {}\nCheckout revision: {checkout:?}",
        source.display()
    );
    for mcu in &inventory.mcus {
        let result = assess_mcu(mcu, &checkout);
        println!(
            "\n{}\n  app: {:?}\n  running: {:?}\n  transport: {:?}\n  eligibility: {:?}\n  revision: {:?}",
            mcu.name, mcu.app, mcu.version, mcu.transport, result.eligibility, result.revision
        );
    }
    ExitCode::SUCCESS
}

fn status_arguments(arguments: Vec<String>) -> Result<(String, PathBuf), String> {
    let mut url = DEFAULT_MOONRAKER_URL.to_owned();
    let mut source = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("klipper"))
        .ok_or("could not determine the invoking user's home directory")?;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--moonraker" => {
                url = arguments
                    .next()
                    .ok_or("--moonraker requires a URL")?
                    .clone()
            }
            "--klipper-source" => {
                source = PathBuf::from(arguments.next().ok_or("--klipper-source requires a path")?)
            }
            _ => return Err(format!("unknown status option {argument:?}")),
        }
    }
    Ok((url, source))
}

fn update(args: Vec<String>) -> ExitCode {
    let (target, options) = match args.split_first() {
        Some((target, options)) if target == "--all" => (None, options),
        Some((target, options)) => (Some(target), options),
        None => return usage("update requires an MCU target or --all"),
    };
    let mut url = DEFAULT_MOONRAKER_URL.to_owned();
    let mut source = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("klipper"));
    let mut workspace = None;
    let mut force = false;
    let mut pull = false;
    let mut it = options.iter();
    while let Some(option) = it.next() {
        match option.as_str() {
            "--force" => force = true,
            "--pull" => pull = true,
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
            "--all" if target.is_none() => {}
            _ => return usage("unknown update option"),
        }
    }
    let Some(source) = source else {
        return usage("could not determine the invoking user's home directory");
    };
    if pull || (io::stdin().is_terminal() && confirm_pull()) {
        let refreshed = match refresh_checkout(&source) {
            Ok(refreshed) => refreshed,
            Err(error) => return fail(error.to_string()),
        };
        println!(
            "Klipper source refresh: {:?} -> {:?}{}",
            refreshed.before,
            refreshed.after,
            if refreshed.advanced {
                " (fast-forwarded)"
            } else {
                " (current)"
            }
        );
    }
    let workspace = workspace
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mcu-update-{}", std::process::id())));
    let inventory = match MoonrakerClient::new(&url).discover_mcus() {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let plan = build_update_plan(&inventory);
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    let selection = match target {
        Some(target) if force => UpdateSelection::Force(target.clone()),
        Some(_) => UpdateSelection::Required,
        None => UpdateSelection::All,
    };
    let offered = inventory
        .mcus
        .iter()
        .filter(|mcu| target.is_none() || Some(&mcu.name) == target)
        .filter(|mcu| is_selected(mcu, &checkout, &selection))
        .map(|mcu| mcu.name.clone())
        .collect::<Vec<_>>();
    if offered.is_empty() {
        println!("no eligible MCUs require an update");
        return ExitCode::SUCCESS;
    }
    let accepted = offered
        .into_iter()
        .filter(|name| {
            eprint!("Update {name}? [y/N] ");
            let _ = io::stderr().flush();
            matches!(read_confirmation().as_deref(), Ok("y") | Ok("yes"))
        })
        .collect::<Vec<_>>();
    if accepted.is_empty() {
        println!("no updates confirmed");
        return ExitCode::SUCCESS;
    }
    let workspace = match RunWorkspace::create(workspace) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let coordinator = BuildCoordinator::new(&source, SystemCommandRunner, SystemCommandRunner);
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
    for name in &accepted {
        let pending = match coordinator.prepare(&inventory, &plan, &workspace, name) {
            Ok(v) => v,
            Err(e) => return fail(e.to_string()),
        };
        match coordinator.execute_and_flash_system(pending.approve(), options.clone()) {
            Ok(v) => {
                println!(
                    "flashed {} bytes from {}",
                    v.flash.padded_bytes,
                    v.artifact.path.display()
                );
                let mcu = inventory
                    .mcus
                    .iter()
                    .find(|mcu| mcu.name == *name)
                    .expect("accepted MCU is discovered");
                if let Err(error) = wait_for_application(mcu) {
                    return fail(error);
                }
            }
            Err(e) => return fail(format!("update failed: {e:?}")),
        }
    }
    if let Err(error) = coordinator.start_after_batch() {
        return fail(error.to_string());
    }
    if let Err(error) = wait_for_mcus(&MoonrakerClient::new(&url), &accepted, &checkout) {
        return fail(error);
    }
    ExitCode::SUCCESS
}

fn wait_for_application(mcu: &mcu_update::moonraker::Mcu) -> Result<(), String> {
    let Some(McuTransport::Serial { device }) = &mcu.transport else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if std::path::Path::new(device).exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{} did not re-enumerate at {device}", mcu.name))
}

fn wait_for_mcus(
    client: &MoonrakerClient,
    selected: &[String],
    checkout: &CheckoutRevision,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(inventory) = client.discover_mcus() {
            let ready = selected.iter().all(|name| {
                inventory
                    .mcus
                    .iter()
                    .find(|mcu| &mcu.name == name)
                    .is_some_and(|mcu| match checkout {
                        CheckoutRevision::Known(revision) => {
                            mcu.version.as_deref() == Some(revision)
                        }
                        CheckoutRevision::Indeterminate => true,
                    })
            });
            if ready {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("Klipper did not reconnect every updated MCU at the built revision".to_owned())
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
        "error: {message}\nusage: mcu-update <inspect> [--moonraker URL]\n       mcu-update status [--moonraker URL] [--klipper-source PATH]\n       mcu-update update <target>|--all [--force] [--pull] [--klipper-source PATH] [--workspace PATH] [--moonraker URL]"
    );
    ExitCode::from(2)
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
