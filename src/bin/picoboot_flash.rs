use std::process::ExitCode;
use std::time::Duration;

use mcu_update::flash::picoboot::bootstrap_system_serial;

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(device), Some(path)) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: picoboot_flash <running-usb-serial-device> <klipper.uf2>");
        return ExitCode::FAILURE;
    };
    let firmware = match std::fs::read(&path) {
        Ok(firmware) => firmware,
        Err(error) => {
            eprintln!("could not read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    match bootstrap_system_serial(
        device.as_ref(),
        &firmware,
        Duration::from_secs(10),
        Duration::from_millis(50),
    ) {
        Ok(result) => {
            println!("flashed {} bytes", result.padded_bytes);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error:?}");
            ExitCode::FAILURE
        }
    }
}
