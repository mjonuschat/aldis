//! Installing Klipper's Linux host MCU the way its `make flash` does: replace
//! the host binary and restart its systemd service. Nothing is flashed.

use std::fmt;
use std::path::PathBuf;

use crate::build::{BuildCommand, CommandError, CommandOutput, CommandPort, CommandStdin};
use crate::flash::FlashResult;

/// Where Klipper's `scripts/flash-linux.sh` installs the host MCU binary.
pub const HOST_MCU_BINARY: &str = "/usr/local/bin/klipper_mcu";
/// The systemd unit `scripts/flash-linux.sh` restarts after installing.
pub const HOST_MCU_UNIT_FILE: &str = "/etc/systemd/system/klipper-mcu.service";
/// The privileged install command; the firmware arrives on stdin so the
/// sudoers entry needs no wildcard for the per-run artifact path.
pub const INSTALL_COMMAND: [&str; 5] = [
    "/usr/bin/install",
    "-m",
    "0755",
    "/dev/stdin",
    HOST_MCU_BINARY,
];
/// The privileged restart command.
pub const RESTART_COMMAND: [&str; 3] = ["/bin/systemctl", "restart", "klipper-mcu"];

const ELFCLASS64: u8 = 2;
#[cfg(target_arch = "aarch64")]
const ELFCLASS32: u8 = 1;
#[cfg(target_arch = "aarch64")]
const EM_ARM: u16 = 40;
#[cfg(target_arch = "aarch64")]
const EM_AARCH64: u16 = 183;
#[cfg(target_arch = "x86_64")]
const EM_X86_64: u16 = 62;

/// Klipper builds `klipper.elf` with the host compiler, so a correct image
/// targets a userland this CPU runs. A 64-bit ARM kernel commonly hosts a
/// 32-bit Raspberry Pi OS userland, while aldis itself ships as aarch64.
#[cfg(target_arch = "aarch64")]
const HOST_ELF_TARGETS: &[(u8, u16)] = &[(ELFCLASS64, EM_AARCH64), (ELFCLASS32, EM_ARM)];
#[cfg(target_arch = "x86_64")]
const HOST_ELF_TARGETS: &[(u8, u16)] = &[(ELFCLASS64, EM_X86_64)];

/// Whether an MCU's Kconfig selects Klipper's Linux host machine.
pub fn is_linux_host(kconfig: &str) -> bool {
    kconfig
        .lines()
        .any(|line| line.trim() == "CONFIG_MACH_LINUX=y")
}

fn is_host_elf(firmware: &[u8]) -> bool {
    let Some(identity) = firmware.get(..20) else {
        return false;
    };
    let class = identity[4];
    let machine = u16::from_le_bytes([identity[18], identity[19]]);
    let header_len = if class == ELFCLASS64 { 64 } else { 52 };
    identity.starts_with(b"\x7fELF")
        && identity[5] == 1
        && firmware.len() >= header_len
        && HOST_ELF_TARGETS.contains(&(class, machine))
}

/// The install step a failure happened in, rendered as the resulting state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallStep {
    /// Replacing the binary; afterwards the old binary may be gone or incomplete.
    Install,
    /// Flushing the new binary to disk; it is installed but may not be durable.
    Sync,
    /// Restarting the service; the new binary is installed.
    Restart,
}

impl fmt::Display for InstallStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Install => write!(f, "could not install {HOST_MCU_BINARY}"),
            Self::Sync => write!(
                f,
                "installed {HOST_MCU_BINARY} but could not flush it to disk"
            ),
            Self::Restart => write!(
                f,
                "installed {HOST_MCU_BINARY} but could not restart klipper-mcu"
            ),
        }
    }
}

/// Failure while installing the Linux host MCU.
#[derive(Debug, thiserror::Error)]
pub enum LinuxHostError {
    /// The image is not an ELF executable for this host.
    #[error("the host MCU firmware is not an ELF executable for this host")]
    NotHostElf,
    /// The systemd unit that runs the host MCU is not installed.
    #[error("the klipper-mcu service is not installed ({})", .0.display())]
    ServiceNotInstalled(PathBuf),
    /// A step's command could not be run at all.
    #[error("{step}")]
    CommandPort {
        /// The step that failed.
        step: InstallStep,
        /// The runner failure.
        #[source]
        source: CommandError,
    },
    /// A step's command ran and failed.
    #[error("{step}: {}", failure_detail(.output))]
    CommandFailed {
        /// The step that failed.
        step: InstallStep,
        /// Captured command output.
        output: Box<CommandOutput>,
    },
}

fn failure_detail(output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.contains("a password is required") {
        format!("{stderr}; run sudo aldis setup")
    } else {
        stderr
    }
}

/// Replaces the host MCU binary and restarts its service through `sudo -n`.
pub struct HostMcuInstaller<R> {
    runner: R,
    unit_file: PathBuf,
}

impl<R: CommandPort> HostMcuInstaller<R> {
    /// Creates an installer that runs its commands through `runner` and
    /// requires the systemd unit at `unit_file`.
    pub fn new(runner: R, unit_file: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            unit_file: unit_file.into(),
        }
    }

    /// Installs `firmware` and restarts the host MCU service.
    pub fn install(&self, firmware: &[u8]) -> Result<FlashResult, LinuxHostError> {
        if !is_host_elf(firmware) {
            return Err(LinuxHostError::NotHostElf);
        }
        if !self.unit_file.exists() {
            return Err(LinuxHostError::ServiceNotInstalled(self.unit_file.clone()));
        }
        self.step(InstallStep::Install, sudo(&INSTALL_COMMAND, Some(firmware)))?;
        self.step(InstallStep::Sync, command("sync", &[], None))?;
        self.step(InstallStep::Restart, sudo(&RESTART_COMMAND, None))?;
        Ok(FlashResult {
            reported_pages: None,
            padded_bytes: firmware.len(),
        })
    }

    fn step(&self, step: InstallStep, command: BuildCommand) -> Result<(), LinuxHostError> {
        let output = self
            .runner
            .run(&command)
            .map_err(|source| LinuxHostError::CommandPort { step, source })?;
        if output.success {
            Ok(())
        } else {
            Err(LinuxHostError::CommandFailed {
                step,
                output: Box::new(output),
            })
        }
    }
}

fn sudo(arguments: &[&str], stdin: Option<&[u8]>) -> BuildCommand {
    let arguments: Vec<&str> = std::iter::once("-n")
        .chain(arguments.iter().copied())
        .collect();
    command("sudo", &arguments, stdin)
}

fn command(program: &str, arguments: &[&str], stdin: Option<&[u8]>) -> BuildCommand {
    BuildCommand {
        program: program.to_owned(),
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        current_dir: None,
        stdin: stdin.map(|bytes| CommandStdin(bytes.to_vec())),
    }
}
