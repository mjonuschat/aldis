use std::process::ExitCode;

use mcu_update::flash::stm32_dfu::{Stm32DfuDevice, find_device};

fn main() -> ExitCode {
    match find_device(Stm32DfuDevice::ROM_BOOTLOADER) {
        Ok((_device, interface)) => {
            println!("STM32 ROM DFU device found on interface {interface}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error:?}");
            ExitCode::FAILURE
        }
    }
}
