use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::coordinator::BuildCoordinator;
use aldis::eligibility::{CheckoutRevision, Eligibility, classify_mcu};
use aldis::logging::LoggingCommandAdapter;
use aldis::moonraker::{McuInventory, MoonrakerAdapter};
use aldis::update_run::{Operation, failure_detail, failure_message};
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
    let moonraker = MoonrakerAdapter::new(moonraker_url);
    let unit = aldis::host::verify_host(moonraker_url, &moonraker)
        .context("refusing to flash firmware")?;
    ui.action("discovering MCUs from Moonraker");
    let inventory = crate::discovery::discover_mcus_with_retry(&moonraker)
        .context("failed to discover MCUs from Moonraker")?;
    arguments.target = crate::discovery::resolve_target_name(&inventory, &arguments.target);
    if let Some(reason) = flash_refusal(&inventory, &arguments.target) {
        ui.action(&format!("error: {reason}"));
        anyhow::bail!(reason);
    }

    crate::update::bridge_refusal(
        arguments.config.as_deref(),
        moonraker_url,
        &unit,
        &inventory,
        std::slice::from_ref(&arguments.target),
    )?;

    let workspace_root =
        crate::default_run_workspace().context("could not create the run workspace")?;
    let workspace = RunWorkspace::adopt(workspace_root);
    let coordinator = BuildCoordinator::with_unit(
        &source,
        LoggingCommandAdapter::new(SystemCommandAdapter),
        LoggingCommandAdapter::new(SystemCommandAdapter),
        unit,
    );
    let pending = coordinator
        .prepare(&inventory, &workspace, &arguments.target, false)
        .with_context(|| format!("failed to prepare {} for flashing", arguments.target))?;

    if !arguments.force
        && !ui.confirm(&confirmation_prompt(
            &arguments.target,
            &arguments.firmware,
            firmware.len(),
        ))
    {
        ui.block("\nflash cancelled\n");
        return Ok(());
    }

    let options = aldis::update_run::standard_flash_options();
    let target_kconfig = pending.kconfig().to_owned();
    let mcu = inventory
        .mcus
        .iter()
        .find(|mcu| mcu.name == arguments.target)
        .context("the prepared MCU is missing from the inventory")?;
    aldis::update_run::ensure_printer_idle(&moonraker).map_err(|message| {
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
            let message = failure_message(
                Operation::Flash,
                failure_detail(Operation::Flash, error),
                restore,
            );
            ui.action(&format!("error: {message}"));
            return Err(anyhow::anyhow!(message));
        }
    }

    // The same post-flash steps as `update`: Klipper must not start before the device is back,
    // and success is only reported once Klipper sees the MCU again.
    ui.begin("waiting for the MCU to restart");
    if let Err(message) = aldis::update_run::wait_for_application(mcu) {
        ui.finish_failure();
        let message = format!(
            "{} successfully but {message}. Klipper may still be stopped; fix the issue, then restart it manually",
            crate::ui::transfer_verb(&target_kconfig),
        );
        ui.action(&format!("error: {message}"));
        return Err(anyhow::anyhow!(message));
    }
    ui.finish_success("MCU restarted");

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
    ui.finish_success("Klipper started");

    ui.begin("waiting for the MCU to reconnect");
    // An explicit firmware file may be any revision, so only reconnection is verified.
    if let Err(message) = aldis::update_run::wait_for_mcus(
        &moonraker,
        std::slice::from_ref(&arguments.target),
        &CheckoutRevision::Indeterminate,
    ) {
        ui.finish_failure();
        let message = format!(
            "{} successfully and Klipper started, but {message}",
            crate::ui::transfer_verb(&target_kconfig),
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

    use super::{confirmation_prompt, flash_refusal};
    use aldis::moonraker::{Mcu, McuInventory};
    use aldis::update_run::{Operation, failure_message};

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
    fn flash_failures_use_the_flash_wording_of_the_shared_message() {
        assert_eq!(
            failure_message(Operation::Flash, "DFU said no".to_owned(), Some(Ok(()))),
            "flash failed: DFU said no. Klipper has been left in its state from before this flash attempt"
        );
        assert!(
            failure_message(Operation::Flash, "DFU said no".to_owned(), None)
                .starts_with("flash failed: DFU said no. Klipper may still be stopped")
        );
    }
}
