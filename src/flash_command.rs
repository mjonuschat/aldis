use std::path::Path;
use std::process::ExitCode;

use anyhow::Context;

use aldis::build::SystemCommandAdapter;
use aldis::coordinator::BuildCoordinator;
use aldis::eligibility::{Eligibility, TargetVersions, classify_mcu};
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
    // Prebuilt firmware (FOCI, Beacon) takes only a file that suits the MCU, sent by the
    // protocol its vendor allows; refuse anything else before the MCU is touched.
    let prebuilt = match inventory
        .mcus
        .iter()
        .find(|mcu| mcu.name == arguments.target)
    {
        Some(mcu) => aldis::prebuilt::prepare_explicit(mcu, &firmware)
            .map_err(|error| match error {
                aldis::prebuilt::ExplicitError::NotBeaconImage(_) => anyhow::Error::new(error)
                    .context(format!(
                        "refusing to flash {}: {} is not a Beacon firmware image",
                        arguments.target,
                        arguments.firmware.display()
                    )),
                error => anyhow::anyhow!("refusing to flash {}: {error}", arguments.target),
            })
            .inspect_err(|error| ui.action(&format!("error: {error:#}")))?,
        None => None,
    };

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
    let flashed = match &prebuilt {
        Some(request) => {
            coordinator.flash_prebuilt(pending.approve(), request, options, |progress| {
                ui.progress_without_build(progress)
            })
        }
        None => coordinator.flash_firmware_system_with_progress(
            pending.approve(),
            &firmware,
            options,
            arguments.force,
            |progress| ui.progress_without_build(progress),
        ),
    };
    match flashed {
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
        &TargetVersions::indeterminate(),
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
    if aldis::foci::is_foci(mcu) && aldis::foci::Board::from_mcu(&mcu.mcu).is_none() {
        return Some(format!(
            "refusing to flash {target}: FOCI publishes no builds for {} boards, so aldis cannot \
             tell where its firmware must start",
            mcu.mcu
        ));
    }
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
                build_versions: None,
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
    fn refuses_to_flash_foci_on_a_board_without_a_known_application_start() {
        let mut inventory = inventory_with("");
        let foci = &mut inventory.mcus[0];
        foci.app = Some("FOCI".to_owned());
        foci.mcu = "stm32g474xx".to_owned();

        let reason = flash_refusal(&inventory, "mcu xiao").expect("an unknown FOCI board");
        assert!(reason.contains("stm32g474xx"), "{reason}");

        inventory.mcus[0].mcu = "stm32h723xx".to_owned();
        assert_eq!(flash_refusal(&inventory, "mcu xiao"), None);
    }

    #[test]
    fn words_the_flash_confirmation_exactly() {
        assert_eq!(
            confirmation_prompt("mcu toolhead", &PathBuf::from("custom.bin"), 4096),
            "Flash custom.bin (4096 bytes) to mcu toolhead? This skips Klipper's Kconfig \
             validation."
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
