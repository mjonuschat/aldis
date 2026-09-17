use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use aldis::flash::katapult::bootstrap::leave_serial;
use aldis::flash::picoboot;
use aldis::flash::reboot::{
    DetectedBootloader, detect_bootloaders, wait_for_any_bootloader, wait_until_absent,
};
use aldis::flash::stm32_dfu::{
    ApplicationProbeResult, Stm32DfuDevice, leave_system_at_path, probe_application_start_at_path,
};
use aldis::flash::usb_bootloader::UsbBootloaderKind;

use crate::cli::RebootArgs;
use crate::ui::{UpdateUi, read_confirmation};

const USB_SYSFS_ROOT: &str = "/sys/bus/usb/devices";
const KATAPULT_BAUD_RATE: u32 = 250_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WATCH_TIMEOUT: Duration = Duration::from_secs(10);
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn reboot(arguments: RebootArgs, mut ui: UpdateUi) -> ExitCode {
    let root = Path::new(USB_SYSFS_ROOT);
    let mut detected = detect_bootloaders(root);
    if detected.is_empty() {
        detected = wait_for_any_bootloader(root, WATCH_TIMEOUT, WATCH_POLL_INTERVAL);
    }
    if detected.is_empty() {
        ui.block("no bootloader detected; power-cycle the board and try again\n");
        return ExitCode::SUCCESS;
    }

    let mut all_resolved = true;
    for bootloader in detected {
        if !leave_one(&bootloader, arguments.yes, &mut ui) {
            all_resolved = false;
        }
    }

    if all_resolved {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Handles one detected bootloader; returns whether it ended up resolved
/// (left cleanly, or correctly reported as needing a manual power cycle).
fn leave_one(bootloader: &DetectedBootloader, skip_prompt: bool, ui: &mut UpdateUi) -> bool {
    let observed = &bootloader.observed;
    ui.heading(&format!(
        "{:?} bootloader at {} ({})",
        bootloader.kind,
        observed.usb_id,
        observed.sysfs_path.display()
    ));

    match bootloader.kind {
        UsbBootloaderKind::Katapult => {
            let Some(serial_device) = &observed.serial_device else {
                ui.block("  no unique serial device at this topology; power-cycle the board\n");
                return false;
            };
            if !confirm(ui, "Reboot into application?", skip_prompt) {
                return true;
            }
            let result = leave_serial(serial_device, KATAPULT_BAUD_RATE, READ_TIMEOUT);
            report_leave_result(result, &observed.sysfs_path, ui)
        }
        UsbBootloaderKind::PicoBoot => {
            if !confirm(ui, "Reboot into application?", skip_prompt) {
                return true;
            }
            let result = picoboot::reboot_at_path(&observed.sysfs_path);
            report_leave_result(result, &observed.sysfs_path, ui)
        }
        UsbBootloaderKind::Stm32Dfu => leave_stm32_dfu(&observed.sysfs_path, skip_prompt, ui),
    }
}

fn leave_stm32_dfu(sysfs_path: &Path, skip_prompt: bool, ui: &mut UpdateUi) -> bool {
    let identity = Stm32DfuDevice::ROM_BOOTLOADER;
    let application_start = match probe_application_start_at_path(identity, sysfs_path) {
        Ok(ApplicationProbeResult::Found(address)) => address,
        Ok(ApplicationProbeResult::NotFound) => {
            ui.block("  no application found in flash; power-cycle the board\n");
            return false;
        }
        Ok(ApplicationProbeResult::Ambiguous(_)) => {
            ui.block("  more than one candidate application offset found; power-cycle the board\n");
            return false;
        }
        Err(error) => {
            ui.block(&format!("  could not probe for an application: {error}\n"));
            return false;
        }
    };
    if !confirm(
        ui,
        &format!("Candidate application found at offset {application_start:#x} — reboot into it?"),
        skip_prompt,
    ) {
        return true;
    }
    let result = leave_system_at_path(identity, sysfs_path, application_start);
    report_leave_result(result, sysfs_path, ui)
}

fn report_leave_result(
    result: Result<(), impl fmt::Display>,
    sysfs_path: &Path,
    ui: &mut UpdateUi,
) -> bool {
    match result {
        Ok(()) => watch_for_departure(sysfs_path, ui),
        Err(error) => {
            ui.block(&format!("  reboot failed: {error}\n"));
            false
        }
    }
}

fn watch_for_departure(sysfs_path: &Path, ui: &mut UpdateUi) -> bool {
    if wait_until_absent(sysfs_path, WATCH_TIMEOUT, WATCH_POLL_INTERVAL) {
        ui.block("  rebooted\n");
        true
    } else {
        ui.block("  still present; power-cycle the board\n");
        false
    }
}

fn confirm(ui: &mut UpdateUi, text: &str, skip_prompt: bool) -> bool {
    if skip_prompt {
        return true;
    }
    ui.prompt(text);
    matches!(read_confirmation().as_deref(), Ok("y") | Ok("yes"))
}
