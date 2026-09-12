//! BOSSA-compatible SAM-BA firmware flashing through `bossac`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::build::{BuildCommand, CommandError, CommandOutput, CommandRunner, SystemCommandRunner};
use crate::flash::FlashResult;

/// SAMD flash placement required by BOSSA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BossaTarget {
    /// Application offset after the installed bootloader.
    pub offset: u32,
}

/// A native BOSSA transfer failure.
#[derive(Debug)]
pub enum BossaError {
    /// Embedded Kconfig did not select exactly one BOSSA-compatible flash start.
    InvalidFlashStartConfiguration,
    /// The temporary firmware file could not be written or removed.
    Io {
        /// The operation being attempted.
        action: &'static str,
        /// The underlying filesystem failure.
        source: std::io::Error,
    },
    /// The command runner could not invoke `bossac`.
    CommandRunner {
        /// The command that could not be invoked.
        command: BuildCommand,
        /// The underlying runner failure.
        source: CommandError,
    },
    /// `bossac` returned an unsuccessful exit status.
    CommandFailed {
        /// The command that failed.
        command: Box<BuildCommand>,
        /// Captured command output for an actionable report.
        output: Box<CommandOutput>,
    },
}

/// Derives the BOSSA application offset from Klipper's embedded Kconfig.
pub fn target_from_kconfig(kconfig: &str) -> Result<BossaTarget, BossaError> {
    let offsets = [
        ("CONFIG_SAMD_FLASH_START_2000=y", 0x2000),
        ("CONFIG_SAMD_FLASH_START_4000=y", 0x4000),
    ];
    let selected = offsets
        .iter()
        .filter(|(symbol, _)| kconfig.lines().any(|line| line.trim() == *symbol))
        .map(|(_, offset)| *offset)
        .collect::<Vec<_>>();
    if selected.len() != 1 {
        return Err(BossaError::InvalidFlashStartConfiguration);
    }
    Ok(BossaTarget {
        offset: selected[0],
    })
}

/// Builds the `bossac` invocation used for SAMD bootloader flashing.
pub fn bossac_command(
    bossac_program: &Path,
    serial_device: &Path,
    target: BossaTarget,
    firmware_path: &Path,
) -> BuildCommand {
    BuildCommand {
        program: bossac_program.to_string_lossy().into_owned(),
        arguments: vec![
            "-U".to_owned(),
            "-p".to_owned(),
            serial_device.to_string_lossy().into_owned(),
            format!("--offset=0x{:x}", target.offset),
            "-b".to_owned(),
            "-R".to_owned(),
            "-w".to_owned(),
            firmware_path.to_string_lossy().into_owned(),
            "-v".to_owned(),
        ],
        current_dir: None,
    }
}

/// Writes one firmware image through `bossac`.
pub fn flash_system(
    bossac_program: &Path,
    serial_device: &Path,
    target: BossaTarget,
    firmware: &[u8],
) -> Result<FlashResult, BossaError> {
    flash_system_with_runner(
        SystemCommandRunner,
        bossac_program,
        serial_device,
        target,
        firmware,
    )
}

/// Writes one firmware image through `bossac` using an injected command runner.
pub fn flash_system_with_runner<R: CommandRunner>(
    runner: R,
    bossac_program: &Path,
    serial_device: &Path,
    target: BossaTarget,
    firmware: &[u8],
) -> Result<FlashResult, BossaError> {
    let firmware_path = temporary_firmware_path();
    fs::write(&firmware_path, firmware).map_err(|source| BossaError::Io {
        action: "write temporary BOSSA firmware",
        source,
    })?;
    let result = run_bossac(
        runner,
        bossac_program,
        serial_device,
        target,
        &firmware_path,
    );
    let cleanup = fs::remove_file(&firmware_path).map_err(|source| BossaError::Io {
        action: "remove temporary BOSSA firmware",
        source,
    });
    result.and(cleanup)?;
    Ok(FlashResult {
        reported_pages: None,
        padded_bytes: firmware.len(),
    })
}

fn run_bossac<R: CommandRunner>(
    runner: R,
    bossac_program: &Path,
    serial_device: &Path,
    target: BossaTarget,
    firmware_path: &Path,
) -> Result<(), BossaError> {
    let command = bossac_command(bossac_program, serial_device, target, firmware_path);
    let output = runner
        .run(&command)
        .map_err(|source| BossaError::CommandRunner {
            command: command.clone(),
            source,
        })?;
    if output.success {
        Ok(())
    } else {
        Err(BossaError::CommandFailed {
            command: Box::new(command),
            output: Box::new(output),
        })
    }
}

fn temporary_firmware_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mcu-update-bossa-{}-{nonce}.bin",
        std::process::id()
    ))
}
