use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, process::Command};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use mcu_update::build::SystemCommandRunner;
use mcu_update::checkout::{
    RefreshResult, refresh as refresh_checkout, revision as checkout_revision,
};
use mcu_update::coordinator::{BuildCoordinator, FlashCoordinatorError, UpdateProgress};
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
    /// When to use ANSI color in interactive update output.
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,
    /// Disable animated progress indicators.
    #[arg(long, global = true)]
    no_progress: bool,
    #[command(subcommand)]
    command: CliCommand,
}

/// Controls ANSI color in interactive output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum ColorMode {
    /// Use color only when stderr is a terminal and NO_COLOR is not set.
    #[default]
    Auto,
    /// Always emit ANSI color sequences.
    Always,
    /// Never emit ANSI color sequences.
    Never,
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
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .args(["targets", "all", "auto"])
))]
struct UpdateArgs {
    /// One or more MCU names reported by Moonraker.
    #[arg(value_name = "MCU", num_args = 1.., conflicts_with_all = ["all", "auto"])]
    targets: Vec<String>,
    /// Update every eligible MCU.
    #[arg(long, conflicts_with_all = ["targets", "auto"])]
    all: bool,
    /// Fast-forward, then update every outdated supported MCU without prompts.
    #[arg(long, conflicts_with_all = ["targets", "all", "force", "pull"])]
    auto: bool,
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
    let cli = Cli::parse();
    match cli.command {
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
        CliCommand::Update(arguments) => {
            update(arguments, UpdateUi::new(cli.color, cli.no_progress))
        }
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
    print!("{}", format_status(&source, &checkout, &inventory, None));
    ExitCode::SUCCESS
}

fn format_status(
    source: &std::path::Path,
    checkout: &CheckoutRevision,
    inventory: &McuInventory,
    refreshed: Option<&RefreshResult>,
) -> String {
    let revision = refreshed.map_or_else(|| checkout_label(checkout).to_owned(), refresh_label);
    let mut output = format!(
        "Klipper source:    {}\nCheckout revision: {}\n",
        source.display(),
        revision,
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

struct UpdateUi {
    color: bool,
    interactive: bool,
    active: Option<ActiveProgress>,
}

struct ActiveProgress {
    label: String,
    spinner: Option<Spinner>,
}

struct Spinner {
    stopped: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl UpdateUi {
    fn new(color_mode: ColorMode, no_progress: bool) -> Self {
        let interactive = io::stderr().is_terminal();
        Self {
            color: colors_enabled(color_mode, interactive),
            interactive: interactive && !no_progress,
            active: None,
        }
    }

    fn block(&mut self, text: &str) {
        self.clear_active();
        eprint!("{text}");
        let _ = io::stderr().flush();
    }

    fn heading(&mut self, text: &str) {
        self.clear_active();
        eprintln!("\n{}", self.style("1", text));
    }

    fn prompt(&mut self, text: &str) {
        self.clear_active();
        eprint!("\n{} [y/N] ", self.style("1", text));
        let _ = io::stderr().flush();
    }

    fn begin(&mut self, label: impl Into<String>) {
        self.clear_active();
        let label = label.into();
        let spinner = self
            .interactive
            .then(|| Spinner::start(label.clone(), self.color));
        if spinner.is_none() {
            eprintln!("  ..{label}");
        }
        self.active = Some(ActiveProgress { label, spinner });
    }

    fn finish_success(&mut self, message: impl AsRef<str>) {
        if let Some(active) = self.active.take() {
            if let Some(spinner) = active.spinner {
                spinner.stop();
            }
            self.clear_spinner();
            eprintln!("{}", self.success_line(message.as_ref()));
        }
    }

    fn finish_failure(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        if let Some(spinner) = active.spinner {
            spinner.stop();
            self.clear_spinner();
        }
        eprintln!("{}", self.failure_line(&format!("{} failed", active.label)));
    }

    fn clear_active(&mut self) {
        if let Some(active) = self.active.take()
            && let Some(spinner) = active.spinner
        {
            spinner.stop();
            self.clear_spinner();
        }
    }

    fn clear_spinner(&self) {
        if self.interactive {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }

    fn success_line(&self, message: &str) -> String {
        if self.interactive {
            format!("  {} {message}", self.style("32", "✓"))
        } else {
            let marker = if self.color {
                self.style("32", "[ok]")
            } else {
                "[ok]".to_owned()
            };
            if self.color {
                format!("  {marker} {message}")
            } else {
                plain_success_line(message)
            }
        }
    }

    fn failure_line(&self, message: &str) -> String {
        let marker = if self.interactive { "error" } else { "[error]" };
        format!("  {} {message}", self.style("31", marker))
    }

    fn style(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn progress(&mut self, progress: UpdateProgress) {
        match progress {
            UpdateProgress::StoppingKlipper => self.begin("stopping Klipper"),
            UpdateProgress::ConfiguringFirmware => {
                self.finish_success("stopped Klipper");
                self.begin("configuring firmware");
            }
            UpdateProgress::CompilingFirmware => {
                self.finish_success("configured firmware");
                self.begin("compiling firmware");
            }
            UpdateProgress::EnteringBootloader => {
                self.finish_success("compiled firmware");
                self.begin("entering bootloader");
            }
            UpdateProgress::BootloaderReady => self.finish_success("bootloader ready"),
            UpdateProgress::StartingFlash => self.begin("flashing firmware"),
        }
    }
}

impl Drop for UpdateUi {
    fn drop(&mut self) {
        self.clear_active();
    }
}

impl Spinner {
    fn start(label: String, color: bool) -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0;
            while !worker_stopped.load(Ordering::Relaxed) {
                let glyph = if color {
                    format!("\x1b[36m{}\x1b[0m", FRAMES[frame])
                } else {
                    FRAMES[frame].to_owned()
                };
                eprint!("\r\x1b[2K  {glyph} {label}");
                let _ = io::stderr().flush();
                frame = (frame + 1) % FRAMES.len();
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self { stopped, worker }
    }

    fn stop(self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.worker.join();
    }
}

fn colors_enabled(mode: ColorMode, is_terminal: bool) -> bool {
    match mode {
        ColorMode::Auto => is_terminal && std::env::var_os("NO_COLOR").is_none(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

fn plain_success_line(message: &str) -> String {
    format!("  [ok] {message}")
}

fn update(arguments: UpdateArgs, mut ui: UpdateUi) -> ExitCode {
    let source = arguments
        .connection
        .klipper_source
        .unwrap_or_else(default_klipper_source);
    let refreshed =
        if arguments.auto || arguments.pull || (io::stdin().is_terminal() && confirm_pull()) {
            let refreshed = match refresh_checkout(&source) {
                Ok(refreshed) => refreshed,
                Err(error) => return fail(error.to_string()),
            };
            Some(refreshed)
        } else {
            None
        };
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
    ui.block(&format_status(
        &source,
        &checkout,
        &inventory,
        refreshed.as_ref(),
    ));
    let selection = if arguments.all {
        UpdateSelection::All
    } else {
        UpdateSelection::Required
    };
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
    if offered.is_empty() {
        ui.block("\nno eligible MCUs require an update\n");
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
    let mut accepted = Vec::new();
    for name in offered {
        let mcu = inventory
            .mcus
            .iter()
            .find(|mcu| mcu.name == name)
            .expect("offered MCU is discovered");
        let current = mcu.version.as_deref().unwrap_or("unknown");
        let next = checkout_label(&checkout);
        if arguments.auto {
            ui.heading(&format!("Update {name} from {current} to {next}"));
        } else {
            ui.prompt(&format!("Update {name} from {current} to {next}?"));
            if !matches!(read_confirmation().as_deref(), Ok("y") | Ok("yes")) {
                continue;
            }
        }
        let pending = match coordinator.prepare(&inventory, &plan, &workspace, &name) {
            Ok(v) => v,
            Err(e) => return fail(e.to_string()),
        };
        match coordinator.execute_and_flash_system_with_progress(
            pending.approve(),
            options.clone(),
            |progress| ui.progress(progress),
        ) {
            Ok(v) => {
                ui.finish_success(format!("flashed {} bytes", v.flash.padded_bytes));
                ui.begin("waiting for MCU restart");
                if let Err(error) = wait_for_application(mcu) {
                    ui.finish_failure();
                    return fail(error);
                }
                ui.finish_success("MCU restart confirmed");
                accepted.push(name);
            }
            Err(error) => {
                ui.finish_failure();
                return fail(update_failure(error));
            }
        }
    }
    if accepted.is_empty() {
        ui.block("\nno updates confirmed\n");
        return ExitCode::SUCCESS;
    }
    ui.heading("Finishing update");
    ui.begin("starting Klipper");
    if let Err(error) = coordinator.start_after_batch() {
        ui.finish_failure();
        return fail(error.to_string());
    }
    ui.finish_success("Klipper ready");
    ui.begin("waiting for updated MCUs to reconnect");
    if let Err(error) = wait_for_mcus(
        &MoonrakerClient::new(&arguments.connection.moonraker.moonraker),
        &accepted,
        &checkout,
    ) {
        ui.finish_failure();
        return fail(error);
    }
    ui.finish_success("all updated MCUs connected");
    ui.heading(&format!(
        "Update complete: {} MCU(s) updated",
        accepted.len()
    ));
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

fn refresh_label(refreshed: &RefreshResult) -> String {
    let suffix = if refreshed.commits_advanced == 1 {
        "commit"
    } else {
        "commits"
    };
    format!(
        "{} -> {} ({} {suffix})",
        checkout_label(&refreshed.before),
        checkout_label(&refreshed.after),
        refreshed.commits_advanced,
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
        Cli, CliCommand, ColorMode, colors_enabled, format_status, plain_success_line,
        selected_mcus_are_ready, setup_check_report, setup_install_report,
        sudo_policy_allows_service_status, update_failure, wait_for_application_with_timeout,
    };
    use clap::{CommandFactory, Parser};
    use mcu_update::build::{BuildCommand, BuildError, CommandOutput};
    use mcu_update::checkout::RefreshResult;
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
    fn honors_color_mode_without_using_terminal_escape_codes_in_plain_output() {
        assert!(colors_enabled(ColorMode::Always, false));
        assert!(!colors_enabled(ColorMode::Never, true));
        assert!(colors_enabled(ColorMode::Auto, true));
        assert!(!colors_enabled(ColorMode::Auto, false));
        assert_eq!(
            plain_success_line("compiled firmware"),
            "  [ok] compiled firmware"
        );
    }

    #[test]
    fn requires_exactly_one_update_target_selector() {
        assert!(Cli::try_parse_from(["mcu-update", "update"]).is_err());
        assert!(Cli::try_parse_from(["mcu-update", "update", "mcu", "--all"]).is_err());
        assert!(Cli::try_parse_from(["mcu-update", "update", "--all"]).is_ok());
    }

    #[test]
    fn accepts_multiple_mcu_targets_or_noninteractive_auto_updates() {
        let CliCommand::Update(targeted) =
            Cli::try_parse_from(["mcu-update", "update", "mcu", "mcu toolhead"])
                .expect("parse multiple targets")
                .command
        else {
            panic!("expected update command");
        };
        assert_eq!(targeted.targets, ["mcu", "mcu toolhead"]);

        let CliCommand::Update(automatic) = Cli::try_parse_from(["mcu-update", "update", "--auto"])
            .expect("parse automatic update")
            .command
        else {
            panic!("expected update command");
        };
        assert!(automatic.auto);
        assert!(Cli::try_parse_from(["mcu-update", "update", "--auto", "--pull"]).is_err());
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
                None,
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
    fn includes_refresh_details_in_the_aligned_checkout_field() {
        let refreshed = RefreshResult {
            before: CheckoutRevision::Known("v0.12.0-123-deadbeef".to_owned()),
            after: CheckoutRevision::Known("v0.13.0-756-g2d7717e3".to_owned()),
            advanced: true,
            commits_advanced: 4,
        };

        let output = format_status(
            std::path::Path::new("/home/pi/klipper"),
            &refreshed.after,
            &McuInventory { mcus: Vec::new() },
            Some(&refreshed),
        );

        assert_eq!(
            output,
            concat!(
                "Klipper source:    /home/pi/klipper\n",
                "Checkout revision: v0.12.0-123-deadbeef -> v0.13.0-756-g2d7717e3 (4 commits)\n",
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
