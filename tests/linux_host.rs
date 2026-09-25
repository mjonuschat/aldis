use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort, CommandStdin};
use aldis::flash::linux_host::{HostMcuInstaller, InstallStep, LinuxHostError, is_linux_host};

#[cfg(target_arch = "aarch64")]
const HOST_MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const HOST_MACHINE: u16 = 62;

fn elf_for(machine: u16) -> Vec<u8> {
    let mut image = vec![0; 64];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    image[18..20].copy_from_slice(&machine.to_le_bytes());
    image.extend_from_slice(b"host mcu code");
    image
}

fn host_elf() -> Vec<u8> {
    elf_for(HOST_MACHINE)
}

#[test]
fn recognizes_only_an_enabled_linux_machine() {
    assert!(is_linux_host(
        "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_LINUX=y\n"
    ));
    assert!(is_linux_host("  CONFIG_MACH_LINUX=y  \n"));
    assert!(!is_linux_host(
        "# CONFIG_MACH_LINUX is not set\nCONFIG_MACH_STM32=y\n"
    ));
    assert!(!is_linux_host("CONFIG_MACH_LINUXX=y\n"));
    assert!(!is_linux_host(""));
}

#[test]
fn installs_syncs_then_restarts_the_service() {
    let unit = unit_file();
    let firmware = host_elf();
    let runner = FakeRunner::new([Ok(ok()), Ok(ok()), Ok(ok())]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    let result = installer
        .install(&firmware)
        .expect("install should succeed");

    assert_eq!(result.reported_pages, None);
    assert_eq!(result.padded_bytes, firmware.len());
    let commands = runner.commands();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].program, "sudo");
    assert_eq!(
        commands[0].arguments,
        [
            "-n",
            "/usr/bin/install",
            "-m",
            "0755",
            "/dev/stdin",
            "/usr/local/bin/klipper_mcu"
        ]
    );
    assert_eq!(commands[0].stdin, Some(CommandStdin(firmware.clone())));
    assert_eq!(commands[0].current_dir, None);
    assert_eq!(commands[1].program, "sync");
    assert!(commands[1].arguments.is_empty());
    assert_eq!(commands[1].stdin, None);
    assert_eq!(commands[2].program, "sudo");
    assert_eq!(
        commands[2].arguments,
        ["-n", "/bin/systemctl", "restart", "klipper-mcu"]
    );
    assert_eq!(commands[2].stdin, None);
}

#[test]
fn refuses_anything_but_a_complete_host_elf_header_before_running_anything() {
    let unit = unit_file();
    let mut big_endian = host_elf();
    big_endian[5] = 2;
    let mut wrong_class = host_elf();
    wrong_class[4] = 1;
    let rejected: [Vec<u8>; 7] = [
        b"\x00\x20\x00\x20binary".to_vec(),
        b"\x7fELF".to_vec(),
        host_elf()[..20].to_vec(),
        Vec::new(),
        elf_for(0xfffe),
        big_endian,
        wrong_class,
    ];
    for firmware in rejected {
        let runner = FakeRunner::new([]);
        let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

        let error = installer.install(&firmware).unwrap_err();

        assert!(matches!(error, LinuxHostError::NotHostElf), "{firmware:?}");
        assert!(runner.commands().is_empty());
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn accepts_a_32_bit_arm_host_mcu_built_on_a_32_bit_userland() {
    let unit = unit_file();
    let mut firmware = vec![0; 52];
    firmware[..4].copy_from_slice(b"\x7fELF");
    firmware[4] = 1;
    firmware[5] = 1;
    firmware[6] = 1;
    firmware[18..20].copy_from_slice(&40u16.to_le_bytes());
    let runner = FakeRunner::new([Ok(ok()), Ok(ok()), Ok(ok())]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    installer
        .install(&firmware)
        .expect("a 32-bit ARM image runs on a 64-bit ARM kernel");

    assert_eq!(runner.commands().len(), 3);
}

#[test]
fn refuses_when_the_systemd_unit_is_missing() {
    let runner = FakeRunner::new([]);
    let missing = std::env::temp_dir().join("aldis-no-such-klipper-mcu.service");
    let installer = HostMcuInstaller::new(runner.clone(), &missing);

    let error = installer.install(&host_elf()).unwrap_err();

    assert!(matches!(error, LinuxHostError::ServiceNotInstalled(ref path) if *path == missing));
    assert!(error.to_string().contains("klipper-mcu"));
    assert!(runner.commands().is_empty());
}

#[test]
fn tells_the_user_to_run_setup_when_sudo_wants_a_password() {
    let unit = unit_file();
    let runner = FakeRunner::new([Ok(failed(b"sudo: a password is required\n"))]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    let error = installer.install(&host_elf()).unwrap_err();

    assert!(matches!(
        error,
        LinuxHostError::CommandFailed {
            step: InstallStep::Install,
            ..
        }
    ));
    let message = error.to_string();
    assert!(
        message.starts_with("could not install /usr/local/bin/klipper_mcu"),
        "{message}"
    );
    assert!(
        message.contains("sudo: a password is required"),
        "{message}"
    );
    assert!(message.contains("run sudo aldis setup"), "{message}");
    assert_eq!(
        runner.commands().len(),
        1,
        "nothing runs after a failed install"
    );
}

#[test]
fn reports_a_failed_restart_as_installed_but_not_restarted() {
    let unit = unit_file();
    let runner = FakeRunner::new([
        Ok(ok()),
        Ok(ok()),
        Ok(failed(b"Job for klipper-mcu.service failed.\n")),
    ]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    let error = installer.install(&host_elf()).unwrap_err();

    assert!(matches!(
        error,
        LinuxHostError::CommandFailed {
            step: InstallStep::Restart,
            ..
        }
    ));
    let message = error.to_string();
    assert!(
        message
            .starts_with("installed /usr/local/bin/klipper_mcu but could not restart klipper-mcu"),
        "{message}"
    );
    assert!(
        message.contains("Job for klipper-mcu.service failed."),
        "{message}"
    );
}

#[test]
fn reports_an_unrunnable_restart_as_installed_but_not_restarted() {
    let unit = unit_file();
    let runner = FakeRunner::new([
        Ok(ok()),
        Ok(ok()),
        Err(CommandError::Spawn(std::io::Error::other("fork failed"))),
    ]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    let error = installer.install(&host_elf()).unwrap_err();

    assert!(matches!(
        error,
        LinuxHostError::CommandPort {
            step: InstallStep::Restart,
            ..
        }
    ));
    assert!(
        error
            .to_string()
            .starts_with("installed /usr/local/bin/klipper_mcu but could not restart klipper-mcu"),
        "{error}"
    );
}

#[test]
fn does_not_restart_when_sync_fails() {
    let unit = unit_file();
    let runner = FakeRunner::new([Ok(ok()), Ok(failed(b"sync: I/O error\n"))]);
    let installer = HostMcuInstaller::new(runner.clone(), &unit.path);

    let error = installer.install(&host_elf()).unwrap_err();

    assert!(matches!(
        error,
        LinuxHostError::CommandFailed {
            step: InstallStep::Sync,
            ..
        }
    ));
    assert!(
        error
            .to_string()
            .starts_with("installed /usr/local/bin/klipper_mcu but could not flush it to disk")
    );
    assert_eq!(runner.commands().len(), 2);
}

struct UnitFile {
    path: PathBuf,
}

impl Drop for UnitFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unit_file() -> UnitFile {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "aldis-klipper-mcu-{}-{nonce}.service",
        std::process::id()
    ));
    fs::write(&path, "[Service]\n").expect("unit fixture");
    UnitFile { path }
}

fn ok() -> CommandOutput {
    CommandOutput::success()
}

fn failed(stderr: &[u8]) -> CommandOutput {
    CommandOutput {
        success: false,
        stdout: Vec::new(),
        stderr: stderr.to_vec(),
    }
}

type Scripted = Result<CommandOutput, CommandError>;

#[derive(Clone)]
struct FakeRunner {
    commands: Arc<Mutex<Vec<BuildCommand>>>,
    outputs: Arc<Mutex<VecDeque<Scripted>>>,
}

impl FakeRunner {
    fn new(outputs: impl IntoIterator<Item = Scripted>) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            outputs: Arc::new(Mutex::new(outputs.into_iter().collect())),
        }
    }

    fn commands(&self) -> Vec<BuildCommand> {
        self.commands.lock().expect("runner lock").clone()
    }
}

impl CommandPort for FakeRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.commands
            .lock()
            .expect("runner lock")
            .push(command.clone());
        self.outputs
            .lock()
            .expect("runner lock")
            .pop_front()
            .expect("an output is scripted for every command")
    }
}

use std::time::Duration;

use aldis::build::BuildRequest;
use aldis::flash::katapult::system::SystemKatapultOptions;
use aldis::flash::system::{
    SystemFlashError, SystemFlashOptions, SystemFlashProgress,
    flash_prepared_firmware_file_with_progress, flash_prepared_system_with_progress,
};
use aldis::prepare::PreparedBuild;

fn host_prepared_build() -> PreparedBuild {
    PreparedBuild {
        target_name: "mcu host".to_owned(),
        mcu: "linux".to_owned(),
        transport: None,
        request: BuildRequest {
            kconfig: "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_LINUX=y\n".to_owned(),
            config_path: PathBuf::from("/nonexistent/.config"),
            artifact_path: PathBuf::from("/nonexistent/klipper.elf"),
            clean: false,
        },
    }
}

fn flash_options(unit: &UnitFile) -> SystemFlashOptions {
    SystemFlashOptions {
        katapult: SystemKatapultOptions {
            baud_rate: 250_000,
            bootloader_timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(10),
            read_timeout: Duration::from_secs(1),
            can_bootloader_settle: Duration::from_millis(10),
        },
        host_mcu_unit_file: unit.path.clone(),
    }
}

fn programs(runner: &FakeRunner) -> Vec<String> {
    runner
        .commands()
        .iter()
        .map(|command| {
            std::iter::once(command.program.as_str())
                .chain(command.arguments.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

const HOST_INSTALL_SEQUENCE: [&str; 3] = [
    "sudo -n /usr/bin/install -m 0755 /dev/stdin /usr/local/bin/klipper_mcu",
    "sync",
    "sudo -n /bin/systemctl restart klipper-mcu",
];

#[test]
fn dispatches_a_built_host_mcu_to_the_installer_before_any_transport() {
    let unit = unit_file();
    let runner = FakeRunner::new([Ok(ok()), Ok(ok()), Ok(ok())]);
    let mut stages = Vec::new();

    let result = flash_prepared_system_with_progress(
        &host_prepared_build(),
        &host_elf(),
        flash_options(&unit),
        &runner,
        |stage| stages.push(stage),
    )
    .expect("host MCU install should succeed");

    assert_eq!(result.padded_bytes, host_elf().len());
    assert_eq!(programs(&runner), HOST_INSTALL_SEQUENCE);
    assert_eq!(stages, [SystemFlashProgress::Installing]);
}

#[test]
fn dispatches_a_host_mcu_firmware_file_to_the_installer() {
    let unit = unit_file();
    let runner = FakeRunner::new([Ok(ok()), Ok(ok()), Ok(ok())]);
    let mut stages = Vec::new();

    flash_prepared_firmware_file_with_progress(
        &host_prepared_build(),
        &host_elf(),
        flash_options(&unit),
        &runner,
        false,
        |stage| stages.push(stage),
    )
    .expect("host MCU install should succeed");

    assert_eq!(programs(&runner), HOST_INSTALL_SEQUENCE);
    assert_eq!(stages, [SystemFlashProgress::Installing]);
}

#[test]
fn host_mcu_flash_errors_display_the_full_installer_message() {
    let error = SystemFlashError::LinuxHost(LinuxHostError::CommandFailed {
        step: InstallStep::Install,
        output: Box::new(failed(b"sudo: a password is required\n")),
    });

    assert_eq!(
        error.to_string(),
        "could not install /usr/local/bin/klipper_mcu: sudo: a password is required; run sudo aldis setup"
    );
    assert_eq!(aldis::error_chain(&error), error.to_string());
}
