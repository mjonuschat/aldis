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

use aldis::moonraker::{HostPort, KlippyState, MoonrakerAdapter};

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
    if beacons.is_empty() {
        return exit_code(all_resolved);
    }
    let url = &arguments.moonraker.moonraker;
    let moonraker = MoonrakerAdapter::new(url);
    let klipper_source = moonraker
        .host_info()
        .ok()
        .and_then(|info| info.klipper_path)
        .unwrap_or_else(crate::default_klipper_source);
    let checkout =
        aldis::beacon::find_checkout(arguments.beacon_source.as_deref(), Some(&klipper_source));
    let mut recovered = false;
    for bootloader in beacons {
        match recover_beacon(&bootloader, checkout.as_deref(), arguments.yes, &mut ui) {
            Recovery::Recovered => recovered = true,
            Recovery::Declined => {}
            Recovery::Failed => all_resolved = false,
        }
    }
    if recovered {
        restart_klipper(url, &moonraker, arguments.yes, &mut ui);
    }
    exit_code(all_resolved)
}

fn exit_code(all_resolved: bool) -> ExitCode {
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

/// How handling one Beacon bootloader ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Recovery {
    /// It was flashed and runs its application again.
    Recovered,
    /// The prompt was declined; nothing was written.
    Declined,
    /// It could not be recovered, and is still in its bootloader or gone.
    Failed,
}

/// Flashes a Beacon left in its bootloader with the image for its revision from `checkout`,
/// which is the only way its bootloader starts the application again, and waits for the
/// application's serial port.
fn recover_beacon(
    bootloader: &ObservedBootloader,
    checkout: Option<&Path>,
    skip_prompt: bool,
    ui: &mut UpdateUi,
) -> Recovery {
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
            return Recovery::Failed;
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
        return Recovery::Declined;
    }
    ui.begin("flashing Beacon firmware");
    match aldis::flash::system::flash_beacon_recovery(&request, |_| {}) {
        Ok(result) => ui.finish_success(format!("flashed {} bytes", result.padded_bytes)),
        Err(error) => {
            ui.finish_failure();
            ui.block(&format!("  flash failed: {}\n", aldis::error_chain(&error)));
            return Recovery::Failed;
        }
    }
    if !beacon_dfu::wait_until_left(&bootloader.sysfs_path, WATCH_TIMEOUT, WATCH_POLL_INTERVAL) {
        ui.block("  still in its bootloader; power-cycle the Beacon\n");
        return Recovery::Failed;
    }
    if !beacon_dfu::wait_for_application(&bootloader.sysfs_path, WATCH_TIMEOUT, WATCH_POLL_INTERVAL)
    {
        ui.block("  left its bootloader but its serial port did not come back; power-cycle it\n");
        return Recovery::Failed;
    }
    ui.block("  back in its application\n");
    Recovery::Recovered
}

/// Restarts Klipper's firmware connections after a Beacon came back, so Klipper, which could not
/// reach it, connects to it again. Only a local Klipper that is not ready is restarted: one that
/// is ready may be printing, and needs no restart to reach a Beacon it already reports.
fn restart_klipper(
    url: &str,
    moonraker: &(impl HostPort + Restart),
    skip_prompt: bool,
    ui: &mut UpdateUi,
) {
    const MANUAL: &str = "restart Klipper (FIRMWARE_RESTART) to reconnect the Beacon";
    if !aldis::host::is_loopback_url(url) {
        ui.block(&format!("\n{url} is not this host's Moonraker; {MANUAL}\n"));
        return;
    }
    match moonraker.host_info().map(|info| info.state) {
        Ok(KlippyState::Ready) => {
            ui.block("\nKlipper is ready; no restart needed\n");
            return;
        }
        Ok(_) => {}
        Err(error) => {
            ui.block(&format!(
                "\ncould not reach Moonraker at {url}: {}; {MANUAL}\n",
                aldis::error_chain(&error)
            ));
            return;
        }
    }
    if !confirm(
        ui,
        "Restart Klipper (FIRMWARE_RESTART) so it reconnects the Beacon?",
        skip_prompt,
    ) {
        ui.block(&format!("  {MANUAL}\n"));
        return;
    }
    match moonraker.firmware_restart() {
        Ok(()) => ui.block("  Klipper is restarting\n"),
        Err(error) => ui.block(&format!(
            "  restart failed: {}; {MANUAL}\n",
            aldis::error_chain(&error)
        )),
    }
}

/// Restarting Klipper's firmware connections through Moonraker.
trait Restart {
    fn firmware_restart(&self) -> Result<(), aldis::moonraker::MoonrakerError>;
}

impl Restart for MoonrakerAdapter {
    fn firmware_restart(&self) -> Result<(), aldis::moonraker::MoonrakerError> {
        MoonrakerAdapter::firmware_restart(self)
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
    use super::{Recovery, Restart, recover_beacon, restart_klipper};
    use aldis::flash::beacon_dfu::ObservedBootloader;
    use aldis::moonraker::{HostInfo, HostPort, KlippyState, MoonrakerError};
    use std::cell::Cell;

    fn stuck(product: &str) -> ObservedBootloader {
        ObservedBootloader {
            sysfs_path: std::path::PathBuf::from("/nonexistent/1-1"),
            vendor_id: 0x04d8,
            product_id: 0xbea1,
            product: product.to_owned(),
        }
    }

    /// A Moonraker reporting Klippy in `state` (or failing to), counting restarts.
    struct FakeMoonraker {
        state: Option<KlippyState>,
        restarts: Cell<usize>,
    }

    impl FakeMoonraker {
        fn new(state: Option<KlippyState>) -> Self {
            Self {
                state,
                restarts: Cell::new(0),
            }
        }
    }

    impl HostPort for FakeMoonraker {
        fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
            Ok(None)
        }

        fn host_info(&self) -> Result<HostInfo, MoonrakerError> {
            let state = self
                .state
                .clone()
                .ok_or_else(|| MoonrakerError::InvalidResponse("unreachable".to_owned()))?;
            Ok(HostInfo {
                state,
                state_message: String::new(),
                software_version: None,
                klipper_path: None,
                python_path: None,
            })
        }

        fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
            Ok(None)
        }
    }

    impl Restart for FakeMoonraker {
        fn firmware_restart(&self) -> Result<(), MoonrakerError> {
            self.restarts.set(self.restarts.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn restarts_only_a_local_klipper_that_is_not_ready() {
        let mut ui = crate::ui::UpdateUi::new(crate::cli::ColorMode::Never, true);
        let local = "http://127.0.0.1:7125";

        let errored = FakeMoonraker::new(Some(KlippyState::Error));
        restart_klipper(local, &errored, true, &mut ui);
        assert_eq!(errored.restarts.get(), 1);

        let ready = FakeMoonraker::new(Some(KlippyState::Ready));
        restart_klipper(local, &ready, true, &mut ui);
        assert_eq!(ready.restarts.get(), 0, "a ready Klipper may be printing");

        let remote = FakeMoonraker::new(Some(KlippyState::Error));
        restart_klipper("http://printer.local:7125", &remote, true, &mut ui);
        assert_eq!(remote.restarts.get(), 0, "never another host's Klipper");

        let unreachable = FakeMoonraker::new(None);
        restart_klipper(local, &unreachable, true, &mut ui);
        assert_eq!(unreachable.restarts.get(), 0);
    }

    #[test]
    fn leaves_a_beacon_it_cannot_find_an_image_for_unresolved() {
        let mut ui = crate::ui::UpdateUi::new(crate::cli::ColorMode::Never, true);
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            recover_beacon(&stuck("Beacon RevH Bootloader"), None, true, &mut ui),
            Recovery::Failed
        );
        assert_eq!(
            recover_beacon(
                &stuck("Beacon RevH Bootloader"),
                Some(dir.path()),
                true,
                &mut ui
            ),
            Recovery::Failed
        );
    }
}
