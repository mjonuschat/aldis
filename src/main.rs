use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, process::Command};

use clap::{Args, Parser, Subcommand};
use mcu_update::build::SystemCommandRunner;
use mcu_update::checkout::{refresh as refresh_checkout, revision as checkout_revision};
use mcu_update::coordinator::BuildCoordinator;
use mcu_update::coordinator::FlashCoordinatorError;
use mcu_update::eligibility::{
    CheckoutRevision, Eligibility, RevisionStatus, UpdateSelection, assess_mcu, is_selected,
};
use mcu_update::flash::katapult::system::SystemKatapultOptions;
use mcu_update::flash::system::{SystemFlashError, SystemFlashOptions};
use mcu_update::moonraker::{McuInventory, McuTransport, MoonrakerClient};
use mcu_update::plan::build_update_plan;
use mcu_update::workspace::RunWorkspace;

const DEFAULT_MOONRAKER_URL: &str = "http://127.0.0.1:7125";
const UDEV_RULES_PATH: &str = "/etc/udev/rules.d/80-mcu-update.rules";
const SUDOERS_PATH: &str = "/etc/sudoers.d/mcu-update";
const UDEV_RULES: &str = include_str!("../templates/80-mcu-update.rules");

/// Safely update Klipper MCU firmware from its embedded configuration.
#[derive(Debug, Parser)]
#[command(name = "mcu-update", version, about)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Show discovered MCUs and whether their running firmware is current.
    Status(ConnectionArgs),
    /// Show the MCU configuration reported by Moonraker.
    Inspect(MoonrakerArgs),
    /// Build and flash one MCU or every eligible MCU.
    Update(UpdateArgs),
    /// Install or verify the host permissions required for unprivileged updates.
    Setup(SetupArgs),
}

#[derive(Debug, Args)]
struct MoonrakerArgs {
    /// Moonraker API URL.
    #[arg(long, default_value = DEFAULT_MOONRAKER_URL)]
    moonraker: String,
}

#[derive(Debug, Args)]
struct ConnectionArgs {
    #[command(flatten)]
    moonraker: MoonrakerArgs,
    /// Klipper source checkout.
    #[arg(long)]
    klipper_source: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// MCU name reported by Moonraker.
    #[arg(required_unless_present = "all")]
    target: Option<String>,
    /// Update every eligible MCU.
    #[arg(long, conflicts_with = "target")]
    all: bool,
    /// Update even when the MCU already reports the checkout revision.
    #[arg(long)]
    force: bool,
    /// Fast-forward the configured Klipper checkout before assessing MCUs.
    #[arg(long)]
    pull: bool,
    #[command(flatten)]
    connection: ConnectionArgs,
    /// Directory retained for generated Kconfigs and firmware artifacts.
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SetupArgs {
    /// Verify installed host permissions without modifying them.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CliCommand::Status(arguments) => status(arguments),
        CliCommand::Inspect(arguments) => {
            match MoonrakerClient::new(&arguments.moonraker).discover_mcus() {
                Ok(inventory) => {
                    print_inventory(&arguments.moonraker, &inventory);
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error.to_string()),
            }
        }
        CliCommand::Update(arguments) => update(arguments),
        CliCommand::Setup(arguments) => setup(arguments),
    }
}

fn setup(arguments: SetupArgs) -> ExitCode {
    if arguments.check {
        check_setup()
    } else {
        install_setup()
    }
}

fn install_setup() -> ExitCode {
    let Some(user) = std::env::var_os("SUDO_USER").and_then(|user| user.into_string().ok()) else {
        return fail("setup must be run with sudo; run: sudo mcu-update setup".to_owned());
    };
    if !valid_user_name(&user) {
        return fail("SUDO_USER is not a valid account name".to_owned());
    }
    let rules_action = match install_file(UDEV_RULES_PATH, udev_rules(), "udev rules") {
        Ok(action) => action,
        Err(error) => return fail(error),
    };
    let service_policy = sudoers_policy(&user);
    let service_action = match install_file(SUDOERS_PATH, &service_policy, "service policy") {
        Ok(action) => action,
        Err(error) => return fail(error),
    };
    match Command::new("chmod").args(["440", SUDOERS_PATH]).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return fail(format!(
                "could not protect service policy: chmod exited with {status}"
            ));
        }
        Err(error) => return fail(format!("could not protect service policy: {error}")),
    }
    match Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return fail(format!(
                "could not reload udev rules: udevadm exited with {status}"
            ));
        }
        Err(error) => return fail(format!("could not reload udev rules: {error}")),
    }
    println!("{}", setup_install_report(rules_action, service_action));
    ExitCode::SUCCESS
}

fn install_file(path: &str, contents: &str, description: &str) -> Result<&'static str, String> {
    if fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok("already current");
    }
    fs::write(path, contents)
        .map_err(|error| format!("could not install {description}: {error}"))?;
    Ok("installed")
}

fn setup_install_report(rules_action: &str, service_action: &str) -> String {
    format!(
        "mcu-update setup:\n  udev rules: {rules_action}\n  service policy: {service_action}\n  service policy permissions: set to 0440\n  udev rules: reloaded"
    )
}

fn check_setup() -> ExitCode {
    let rules = fs::read_to_string(UDEV_RULES_PATH).is_ok_and(|contents| contents == udev_rules());
    let service = Command::new("sudo")
        .args(["-n", "/bin/systemctl", "is-active", "klipper"])
        .output()
        .is_ok_and(|output| sudo_policy_allows_service_status(&output.stdout));
    println!("{}", setup_check_report(rules, service));
    if rules && service {
        ExitCode::SUCCESS
    } else {
        fail("mcu-update setup is incomplete; run sudo mcu-update setup".to_owned())
    }
}

fn setup_check_report(rules: bool, service: bool) -> String {
    format!(
        "mcu-update setup:\n  udev rules: {}\n  Klipper service access: {}",
        if rules {
            "ready"
        } else {
            "missing or outdated"
        },
        if service { "ready" } else { "unavailable" },
    )
}

fn sudo_policy_allows_service_status(stdout: &[u8]) -> bool {
    matches!(
        std::str::from_utf8(stdout).map(str::trim),
        Ok("active" | "inactive" | "failed" | "activating" | "deactivating" | "reloading")
    )
}

fn valid_user_name(user: &str) -> bool {
    !user.is_empty()
        && user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn udev_rules() -> &'static str {
    UDEV_RULES
}

fn sudoers_policy(user: &str) -> String {
    format!(
        "{user} ALL=(root) NOPASSWD: /bin/systemctl is-active klipper, /bin/systemctl stop klipper, /bin/systemctl start klipper\n"
    )
}

fn status(arguments: ConnectionArgs) -> ExitCode {
    let source = arguments
        .klipper_source
        .unwrap_or_else(default_klipper_source);
    let inventory = match MoonrakerClient::new(&arguments.moonraker.moonraker).discover_mcus() {
        Ok(inventory) => inventory,
        Err(error) => return fail(error.to_string()),
    };
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    print!("{}", format_status(&source, &checkout, &inventory));
    ExitCode::SUCCESS
}

fn format_status(
    source: &std::path::Path,
    checkout: &CheckoutRevision,
    inventory: &McuInventory,
) -> String {
    let mut output = format!(
        "Klipper source:    {}\nCheckout revision: {}\n",
        source.display(),
        checkout_label(checkout),
    );
    for mcu in &inventory.mcus {
        let status = assess_mcu(mcu, checkout);
        output.push_str(&format!("\n{}\n", mcu.name));
        for (label, value) in [
            ("firmware:", firmware_label(mcu)),
            (
                "version:",
                mcu.version.as_deref().unwrap_or("unknown").to_owned(),
            ),
            ("model:", mcu.mcu.clone()),
            ("connection:", connection_label(mcu.transport.as_ref())),
            ("supported:", supported_label(status.eligibility)),
            ("needs update:", update_label(status.revision)),
        ] {
            output.push_str(&format!("  {label:<13} {value}\n"));
        }
    }
    output
}

fn checkout_label(checkout: &CheckoutRevision) -> &str {
    match checkout {
        CheckoutRevision::Known(revision) => revision,
        CheckoutRevision::Indeterminate => "unknown",
    }
}

fn firmware_label(mcu: &mcu_update::moonraker::Mcu) -> String {
    if let Some(app) = mcu.app.as_deref().filter(|app| !app.trim().is_empty()) {
        app.to_owned()
    } else if !mcu.kconfig.trim().is_empty() {
        "Klipper".to_owned()
    } else {
        "unknown".to_owned()
    }
}

fn connection_label(transport: Option<&McuTransport>) -> String {
    match transport {
        Some(McuTransport::Serial { device }) => format!("serial ({device})"),
        Some(McuTransport::Can { interface, uuid }) => format!("CAN ({interface}, {uuid:012x})"),
        None => "not configured".to_owned(),
    }
}

fn supported_label(eligibility: Eligibility) -> String {
    match eligibility {
        Eligibility::Eligible => "yes".to_owned(),
        Eligibility::ExternallyManaged | Eligibility::Unsupported => "no".to_owned(),
    }
}

fn update_label(revision: Option<RevisionStatus>) -> String {
    match revision {
        Some(RevisionStatus::Current) => "no".to_owned(),
        Some(RevisionStatus::UpdateRequired) => "yes".to_owned(),
        Some(RevisionStatus::Indeterminate) => "unknown".to_owned(),
        None => "n/a".to_owned(),
    }
}

fn default_klipper_source() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("klipper"))
        .unwrap_or_else(|| PathBuf::from("klipper"))
}

fn update(arguments: UpdateArgs) -> ExitCode {
    let source = arguments
        .connection
        .klipper_source
        .unwrap_or_else(default_klipper_source);
    if arguments.pull || (io::stdin().is_terminal() && confirm_pull()) {
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
    let workspace = arguments
        .workspace
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mcu-update-{}", std::process::id())));
    let inventory =
        match MoonrakerClient::new(&arguments.connection.moonraker.moonraker).discover_mcus() {
            Ok(v) => v,
            Err(e) => return fail(e.to_string()),
        };
    let plan = build_update_plan(&inventory);
    let checkout = checkout_revision(&source).unwrap_or(CheckoutRevision::Indeterminate);
    let target = arguments.target.as_ref();
    let selection = match target {
        Some(target) if arguments.force => UpdateSelection::Force(target.clone()),
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
            Err(error) => return fail(update_failure(error)),
        }
    }
    if let Err(error) = coordinator.start_after_batch() {
        return fail(error.to_string());
    }
    if let Err(error) = wait_for_mcus(
        &MoonrakerClient::new(&arguments.connection.moonraker.moonraker),
        &accepted,
        &checkout,
    ) {
        return fail(error);
    }
    ExitCode::SUCCESS
}

fn update_failure(error: FlashCoordinatorError<SystemFlashError>) -> String {
    let detail = match error {
        FlashCoordinatorError::Coordinator(error) => error.to_string(),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read the built firmware: {error}")
        }
        FlashCoordinatorError::Flash(error) => format!("native flash failed: {error:?}"),
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
    let deadline = Instant::now() + timeout;
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
        if let Ok(inventory) = client.discover_mcus()
            && selected_mcus_are_ready(&inventory, selected, checkout)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("Klipper did not reconnect every updated MCU at the built revision".to_owned())
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

fn fail(message: String) -> ExitCode {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Cli, format_status, selected_mcus_are_ready, setup_check_report, setup_install_report,
        sudo_policy_allows_service_status, update_failure, wait_for_application_with_timeout,
    };
    use clap::{CommandFactory, Parser};
    use mcu_update::build::{BuildCommand, BuildError, CommandOutput};
    use mcu_update::coordinator::{CoordinatorError, FlashCoordinatorError};
    use mcu_update::eligibility::CheckoutRevision;
    use mcu_update::flash::system::SystemFlashError;
    use mcu_update::moonraker::{Mcu, McuInventory, McuTransport};

    #[test]
    fn top_level_help_describes_the_cli() {
        let help = Cli::command().render_help().to_string();

        assert!(help.contains("Usage:"));
        assert!(help.contains("setup"));
        assert!(help.contains("update"));
    }

    #[test]
    fn requires_exactly_one_update_target_selector() {
        assert!(Cli::try_parse_from(["mcu-update", "update"]).is_err());
        assert!(Cli::try_parse_from(["mcu-update", "update", "mcu", "--all"]).is_err());
        assert!(Cli::try_parse_from(["mcu-update", "update", "--all"]).is_ok());
    }

    #[test]
    fn renders_human_readable_klipper_status() {
        let inventory = McuInventory {
            mcus: vec![mcu(
                "mcu",
                "v0.12.0-123-deadbeef",
                Some("/dev/serial/by-id/mcu"),
            )],
        };

        assert_eq!(
            format_status(
                std::path::Path::new("/home/pi/klipper"),
                &CheckoutRevision::Known("v0.13.0-756-g2d7717e3".to_owned()),
                &inventory,
            ),
            concat!(
                "Klipper source:    /home/pi/klipper\n",
                "Checkout revision: v0.13.0-756-g2d7717e3\n\n",
                "mcu\n",
                "  firmware:     Klipper\n",
                "  version:      v0.12.0-123-deadbeef\n",
                "  model:        test\n",
                "  connection:   serial (/dev/serial/by-id/mcu)\n",
                "  supported:    yes\n",
                "  needs update: yes\n",
            )
        );
    }

    #[test]
    fn recognizes_a_permitted_inactive_klipper_status() {
        assert!(sudo_policy_allows_service_status(b"inactive\n"));
        assert!(!sudo_policy_allows_service_status(
            b"sudo: a password is required\n"
        ));
    }

    #[test]
    fn reports_each_setup_prerequisite() {
        assert_eq!(
            setup_check_report(true, false),
            "mcu-update setup:\n  udev rules: ready\n  Klipper service access: unavailable"
        );
    }

    #[test]
    fn reports_setup_install_actions() {
        assert_eq!(
            setup_install_report("already current", "installed"),
            "mcu-update setup:\n  udev rules: already current\n  service policy: installed\n  service policy permissions: set to 0440\n  udev rules: reloaded"
        );
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
