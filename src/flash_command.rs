use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::coordinator::{BuildCoordinator, FlashCoordinatorError};
use aldis::eligibility::{Eligibility, classify_mcu};
use aldis::flash::system::SystemFlashError;
use aldis::logging::LoggingCommandAdapter;
use aldis::moonraker::{McuInventory, MoonrakerAdapter};
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
    let _lock =
        aldis::lock::UpdateLock::try_acquire_default().context("could not start the flash")?;
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
    let moonraker_url = &arguments.connection.moonraker.moonraker;
    aldis::host::verify_host(moonraker_url, &MoonrakerAdapter::new(moonraker_url))
        .context("refusing to flash firmware")?;
    ui.action("discovering MCUs from Moonraker");
    let inventory = crate::discovery::discover_mcus_with_retry(&MoonrakerAdapter::new(
        &arguments.connection.moonraker.moonraker,
    ))
    .context("failed to discover MCUs from Moonraker")?;
    arguments.target = crate::discovery::resolve_target_name(&inventory, &arguments.target);
    if let Some(reason) = flash_refusal(&inventory, &arguments.target) {
        ui.action(&format!("error: {reason}"));
        anyhow::bail!(reason);
    }

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

    let options = aldis::update_run::standard_flash_options();
    let target_kconfig = pending.kconfig().to_owned();
    aldis::update_run::ensure_printer_idle(&MoonrakerAdapter::new(
        &arguments.connection.moonraker.moonraker,
    ))
    .map_err(|message| {
        ui.action(&format!("error: {message}"));
        anyhow::anyhow!(message)
    })?;
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

fn flash_refusal(inventory: &McuInventory, target: &str) -> Option<String> {
    let mcu = inventory.mcus.iter().find(|mcu| mcu.name == target)?;
    match classify_mcu(mcu) {
        Eligibility::UnsupportedMcu(family) => Some(format!(
            "refusing to flash {target}: {family} boards have no bootloader aldis can flash"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{confirmation_prompt, flash_failure, flash_refusal};
    use aldis::build::{BuildCommand, BuildError, CommandOutput};
    use aldis::coordinator::{CoordinatorError, FlashCoordinatorError};
    use aldis::flash::system::SystemFlashError;
    use aldis::moonraker::{Mcu, McuInventory};

    fn inventory_with(kconfig: &str) -> McuInventory {
        McuInventory {
            mcus: vec![Mcu {
                name: "mcu xiao".to_owned(),
                app: Some("Klipper".to_owned()),
                version: Some("v0.13.0-770-gce7002bed".to_owned()),
                mcu: "samd21g18a".to_owned(),
                canbus_frequency_hz: None,
                transport: None,
                kconfig: kconfig.to_owned(),
            }],
            unreported: Vec::new(),
        }
    }

    #[test]
    fn refuses_to_flash_an_mcu_family_without_a_flashable_bootloader() {
        let reason = flash_refusal(&inventory_with("CONFIG_MACH_ATSAMD=y\n"), "mcu xiao")
            .expect("SAMD21 has no bootloader aldis can drive");

        assert!(reason.contains("mcu xiao"), "{reason}");
        assert!(reason.contains("ATSAMD"), "{reason}");
        assert_eq!(
            flash_refusal(&inventory_with("CONFIG_MACH_STM32=y\n"), "mcu xiao"),
            None
        );
        assert_eq!(
            flash_refusal(&inventory_with("CONFIG_MACH_ATSAMD=y\n"), "mcu other"),
            None
        );
    }

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
