use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use aldis::flash::beacon_dfu::{self, ObservedBootloader};
use aldis::flash::katapult::bootstrap::leave_serial;
use aldis::flash::picoboot;
use aldis::flash::reboot::{
    DetectedBootloader, detect_bootloaders, wait_for_any_bootloader, wait_until_absent,
};
use aldis::flash::samba::{SystemSambaPort, reboot_into_application};
use aldis::flash::stm32_dfu::{
    ApplicationProbeResult, Stm32DfuDevice, leave_system_at_path, probe_application_start_at_path,
};
use aldis::flash::usb_bootloader::UsbBootloaderKind;

use crate::cli::RebootArgs;
use crate::ui::UpdateUi;

const USB_SYSFS_ROOT: &str = "/sys/bus/usb/devices";
const KATAPULT_BAUD_RATE: u32 = 250_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WATCH_TIMEOUT: Duration = Duration::from_secs(10);
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn reboot(arguments: RebootArgs, mut ui: UpdateUi) -> ExitCode {
    let _lock = match aldis::lock::UpdateLock::try_acquire_default() {
        Ok(lock) => lock,
        Err(error) => return crate::fail(format!("could not start the reboot: {error}")),
    };
    let root = Path::new(USB_SYSFS_ROOT);
    let mut detected = detect_bootloaders(root);
    let mut beacons = beacon_dfu::scan_bootloaders(root);
    if detected.is_empty() && beacons.is_empty() {
        detected = wait_for_any_bootloader(root, WATCH_TIMEOUT, WATCH_POLL_INTERVAL);
        beacons = beacon_dfu::scan_bootloaders(root);
    }
    if detected.is_empty() && beacons.is_empty() {
        ui.block("no bootloader detected; power-cycle the board and try again\n");
        return ExitCode::SUCCESS;
    }

    let mut all_resolved = true;
    for bootloader in detected {
        if !leave_one(&bootloader, arguments.yes, &mut ui) {
            all_resolved = false;
        }
    }
    let checkout = (!beacons.is_empty())
        .then(|| {
            aldis::beacon::find_checkout(
                arguments.beacon_source.as_deref(),
                Some(&crate::default_klipper_source()),
            )
        })
        .flatten();
    for bootloader in beacons {
        if !recover_beacon(&bootloader, checkout.as_deref(), arguments.yes, &mut ui) {
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
        UsbBootloaderKind::SamBa => {
            let Some(serial_device) = &observed.serial_device else {
                ui.block("  no unique serial device at this topology; power-cycle the board\n");
                return false;
            };
            if !confirm(ui, "Reboot into application?", skip_prompt) {
                return true;
            }
            let result = SystemSambaPort::open(serial_device, READ_TIMEOUT)
                .map_err(|error| error.to_string())
                .and_then(|port| reboot_into_application(port).map_err(|error| error.to_string()));
            report_leave_result(result, &observed.sysfs_path, ui)
        }
    }
}

/// Flashes a Beacon left in its bootloader with the image for its revision from `checkout`,
/// which is the only way its bootloader starts the application again. Returns whether the
/// Beacon ended up resolved.
fn recover_beacon(
    bootloader: &ObservedBootloader,
    checkout: Option<&Path>,
    skip_prompt: bool,
    ui: &mut UpdateUi,
) -> bool {
    ui.heading(&format!(
        "Beacon bootloader at {} ({})",
        bootloader.usb_id(),
        bootloader.sysfs_path.display()
    ));
    let request = match aldis::prebuilt::prepare_beacon_recovery(checkout, bootloader) {
        Ok(request) => request,
        Err(reason) => {
            ui.block(&format!(
                "  cannot recover it: {reason}; flash it with beacon_klipper's \
                 update_firmware.py\n"
            ));
            return false;
        }
    };
    if !confirm(
        ui,
        &format!(
            "A Beacon leaves its bootloader only by being flashed. Flash {} ({})?",
            request.path().display(),
            aldis::beacon::reported(&request.image().version)
        ),
        skip_prompt,
    ) {
        return true;
    }
    ui.begin("flashing Beacon firmware");
    match aldis::flash::system::flash_beacon_recovery(&request, |_| {}) {
        Ok(result) => ui.finish_success(format!("flashed {} bytes", result.padded_bytes)),
        Err(error) => {
            ui.finish_failure();
            ui.block(&format!("  flash failed: {}\n", aldis::error_chain(&error)));
            return false;
        }
    }
    if beacon_dfu::wait_until_left(&bootloader.sysfs_path, WATCH_TIMEOUT, WATCH_POLL_INTERVAL) {
        ui.block("  left its bootloader; restart Klipper (FIRMWARE_RESTART) to reconnect it\n");
        true
    } else {
        ui.block("  still in its bootloader; power-cycle the Beacon\n");
        false
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
    ui.confirm(text)
}

#[cfg(test)]
mod tests {
    use super::recover_beacon;
    use aldis::flash::beacon_dfu::ObservedBootloader;

    fn stuck(product: &str) -> ObservedBootloader {
        ObservedBootloader {
            sysfs_path: std::path::PathBuf::from("/nonexistent/1-1"),
            vendor_id: 0x04d8,
            product_id: 0xbea1,
            product: product.to_owned(),
        }
    }

    #[test]
    fn leaves_a_beacon_it_cannot_find_an_image_for_unresolved() {
        let mut ui = crate::ui::UpdateUi::new(crate::cli::ColorMode::Never, true);
        let dir = tempfile::tempdir().unwrap();

        assert!(!recover_beacon(
            &stuck("Beacon RevH Bootloader"),
            None,
            true,
            &mut ui
        ));
        assert!(!recover_beacon(
            &stuck("Beacon RevH Bootloader"),
            Some(dir.path()),
            true,
            &mut ui
        ));
    }
}
