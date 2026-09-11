use std::process::ExitCode;

use mcu_update::flash::picoboot::flash_system;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: picoboot_flash <klipper.uf2>");
        return ExitCode::FAILURE;
    };
    let firmware = match std::fs::read(&path) {
        Ok(firmware) => firmware,
        Err(error) => {
            eprintln!("could not read {}: {error}", path.to_string_lossy());
            return ExitCode::FAILURE;
        }
    };
    match flash_system(&firmware) {
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
