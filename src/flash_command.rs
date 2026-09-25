use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::coordinator::{BuildCoordinator, FlashCoordinatorError};
use aldis::flash::katapult::system::SystemKatapultOptions;
use aldis::flash::linux_host::HOST_MCU_UNIT_FILE;
use aldis::flash::system::{SystemFlashError, SystemFlashOptions};
use aldis::logging::LoggingCommandAdapter;
use aldis::moonraker::MoonrakerAdapter;
use aldis::workspace::RunWorkspace;

use crate::cli::FlashArgs;
use crate::fail;
use crate::ui::UpdateUi;

pub(crate) fn flash(arguments: FlashArgs, mut ui: UpdateUi) -> ExitCode {
    match run_flash(arguments, &mut ui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(format!("{error:#}")),
    }
}

fn run_flash(mut arguments: FlashArgs, ui: &mut UpdateUi) -> anyhow::Result<()> {
    let firmware = std::fs::read(&arguments.firmware).with_context(|| {
        format!(
            "could not read firmware file {}",
            arguments.firmware.display()
        )
    })?;
    let source = arguments
        .connection
        .klipper_source
        .clone()
        .unwrap_or_else(crate::default_klipper_source);
    ui.action("discovering MCUs from Moonraker");
    let inventory = crate::discovery::discover_mcus_with_retry(&MoonrakerAdapter::new(
        &arguments.connection.moonraker.moonraker,
    ))
    .context("failed to discover MCUs from Moonraker")?;
    arguments.target = crate::discovery::resolve_target_name(&inventory, &arguments.target);

    let workspace_root =
        crate::default_run_workspace().context("could not create the run workspace")?;
    let workspace = RunWorkspace::adopt(workspace_root);
    let coordinator = BuildCoordinator::new(
        &source,
        LoggingCommandAdapter::new(SystemCommandAdapter),
        LoggingCommandAdapter::new(SystemCommandAdapter),
    );
    let pending = coordinator
        .prepare(&inventory, &workspace, &arguments.target, false)
        .with_context(|| format!("failed to prepare {} for flashing", arguments.target))?;

    if !arguments.force {
        ui.prompt(&confirmation_prompt(
            &arguments.target,
            &arguments.firmware,
            firmware.len(),
        ));
        if !crate::ui::confirmed() {
            ui.block("\nflash cancelled\n");
            return Ok(());
        }
    }

    let options = SystemFlashOptions {
        katapult: SystemKatapultOptions {
            baud_rate: 250_000,
            bootloader_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(50),
            read_timeout: Duration::from_secs(5),
            can_bootloader_settle: Duration::from_millis(100),
        },
        host_mcu_unit_file: PathBuf::from(HOST_MCU_UNIT_FILE),
    };
    let target_kconfig = pending.kconfig().to_owned();
    match coordinator.flash_firmware_system_with_progress(
        pending.approve(),
        &firmware,
        options,
        arguments.force,
        |progress| ui.progress_without_build(progress),
    ) {
        Ok(result) => {
            ui.finish_success(crate::ui::transfer_summary(
                &target_kconfig,
                result.padded_bytes,
            ));
        }
        Err(error) => {
            ui.finish_failure();
            let restore = error
                .allows_klipper_restore()
                .then(|| coordinator.restore_after_failure());
            let message = flash_failure(error, restore);
            ui.action(&format!("error: {message}"));
            return Err(anyhow::anyhow!(message));
        }
    }

    ui.begin("starting Klipper");
    if let Err(error) = coordinator.start_after_batch() {
        ui.finish_failure();
        let message = format!(
            "{} successfully but Klipper failed to restart: {}",
            crate::ui::transfer_verb(&target_kconfig),
            aldis::error_chain(&error)
        );
        ui.action(&format!("error: {message}"));
        return Err(anyhow::anyhow!(message));
    }
    ui.finish_success("Klipper ready");
    ui.action("run completed successfully");
    Ok(())
}

fn confirmation_prompt(target: &str, firmware: &Path, byte_count: usize) -> String {
    format!(
        "Flash {} ({byte_count} bytes) to {target}? This skips Klipper's Kconfig validation.",
        firmware.display()
    )
}

fn flash_failure(
    error: FlashCoordinatorError<SystemFlashError>,
    restore: Option<Result<(), aldis::coordinator::CoordinatorError>>,
) -> String {
    let detail = match error {
        FlashCoordinatorError::Coordinator(error) => aldis::error_chain(&error),
        FlashCoordinatorError::Artifact(error) => {
            format!("could not read the firmware file: {error}")
        }
        FlashCoordinatorError::Flash(error) => error.to_string(),
    };
    match restore {
        None => format!(
            "flash failed: {detail}. Klipper may still be stopped; fix the issue, then restart it manually"
        ),
        Some(Ok(())) => format!(
            "flash failed: {detail}. Klipper has been left in its state from before this flash attempt"
        ),
        Some(Err(restore_error)) => format!(
            "flash failed: {detail}. Klipper also failed to restore: {}; fix the issue, then restart it manually",
            aldis::error_chain(&restore_error)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{confirmation_prompt, flash_failure};
    use aldis::build::{BuildCommand, BuildError, CommandOutput};
    use aldis::coordinator::{CoordinatorError, FlashCoordinatorError};
    use aldis::flash::system::SystemFlashError;

    #[test]
    fn names_the_firmware_file_target_and_byte_count_in_the_confirmation_prompt() {
        let prompt = confirmation_prompt("mcu toolhead", &PathBuf::from("custom.bin"), 4096);

        assert!(prompt.contains("custom.bin"));
        assert!(prompt.contains("4096 bytes"));
        assert!(prompt.contains("mcu toolhead"));
        assert!(prompt.contains("Kconfig validation"));
    }

    #[test]
    fn reports_the_host_mcu_setup_hint_in_the_final_flash_error() {
        use aldis::flash::linux_host::{InstallStep, LinuxHostError};

        let error = FlashCoordinatorError::<SystemFlashError>::Flash(SystemFlashError::LinuxHost(
            LinuxHostError::CommandFailed {
                step: InstallStep::Install,
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: b"sudo: a password is required\n".to_vec(),
                }),
            },
        ));

        let message = flash_failure(error, None);

        assert!(
            message.starts_with(
                "flash failed: could not install /usr/local/bin/klipper_mcu: \
                 sudo: a password is required; run sudo aldis setup"
            ),
            "{message}"
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
                    stdin: None,
                }),
                output: Box::new(CommandOutput {
                    success: false,
                    stdout: b"unrelated output".to_vec(),
                    stderr: b"permission denied".to_vec(),
                }),
            }),
        );

        let message = flash_failure(error, None);

        assert!(message.contains("make failed: permission denied"));
        assert!(!message.contains("unrelated output"));
        assert!(message.contains("may still be stopped"));
    }

    #[test]
    fn reports_that_klipper_was_restored_after_a_pre_flash_failure() {
        let error = FlashCoordinatorError::<SystemFlashError>::Artifact(std::io::Error::other(
            "missing firmware file",
        ));

        let message = flash_failure(error, Some(Ok(())));

        assert!(message.contains("left in its state from before this flash attempt"));
        assert!(!message.contains("may still be stopped"));
    }
}
