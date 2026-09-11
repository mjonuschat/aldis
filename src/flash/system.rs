//! Topology-scoped native flashing dispatch for prepared MCU updates.

use std::io;
use std::path::PathBuf;
use std::thread;

use crate::flash::FlashBackend;
use crate::flash::FlashResult;
use crate::flash::katapult::backend::KatapultFlashError;
use crate::flash::katapult::bootstrap::{CanBootstrapError, request_can_bootloader};
use crate::flash::katapult::can::SocketCanIo;
use crate::flash::katapult::serial::{KatapultSerialTransport, SystemSerialIo, UsbBootloaderError};
use crate::flash::katapult::{backend::KatapultBackend, system::SystemKatapultOptions};
use crate::flash::picoboot::{PicoBootError, flash_system_at_path as flash_picoboot_at_path};
use crate::flash::stm32_dfu::{
    Stm32DfuError, Stm32DfuTarget, flash_system_at_path as flash_stm32_at_path, target_from_kconfig,
};
use crate::flash::usb_bootloader::{
    ObservedUsbBootloader, SelectedUsbBootloader, UsbBootloaderSelectionError,
    select_usb_bootloader,
};
use crate::moonraker::McuTransport;
use crate::prepare::PreparedBuild;

/// A selected USB transfer route bound to the observed USB topology.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SerialFlashRoute {
    /// Katapult's serial bootloader device.
    Katapult { serial_device: PathBuf },
    /// STM32 ROM DFU and its Kconfig-derived application target.
    Stm32Dfu {
        /// USB topology retained across re-enumeration.
        sysfs_path: PathBuf,
        /// Application placement derived from the prepared Kconfig.
        target: Stm32DfuTarget,
    },
    /// RP2040/RP2350 PicoBoot at one USB topology.
    PicoBoot { sysfs_path: PathBuf },
}

/// A selected update cannot be dispatched to a native backend.
#[derive(Debug)]
pub enum SystemFlashError {
    /// The prepared target did not retain a configured transport.
    MissingTransport,
    /// USB bootloader entry or topology observation failed.
    Bootloader(UsbBootloaderError),
    /// The observed USB identity has no safe native backend.
    Selection(UsbBootloaderSelectionError),
    /// The observed STM32 route has no valid Kconfig application address.
    Stm32Target(Stm32DfuError),
    /// The Katapult USB serial device could not be opened.
    KatapultOpen(serialport::Error),
    /// Katapult rejected or could not verify its USB serial transfer.
    KatapultFlash(KatapultFlashError),
    /// STM32 ROM DfuSe flashing failed.
    Stm32Dfu(Stm32DfuError),
    /// PicoBoot flashing failed.
    PicoBoot(PicoBootError),
    /// The CAN socket could not be opened.
    CanSocket(io::Error),
    /// Katapult CAN bootloader entry or node assignment failed.
    Can(CanBootstrapError<io::Error>),
    /// Katapult rejected or could not verify its CAN transfer.
    CanFlash(KatapultFlashError),
}

/// Derives exactly one native USB route from one observed bootloader.
pub fn serial_route(
    observed: ObservedUsbBootloader,
    kconfig: &str,
) -> Result<SerialFlashRoute, SystemFlashError> {
    match select_usb_bootloader(observed).map_err(SystemFlashError::Selection)? {
        SelectedUsbBootloader::Katapult { serial_device, .. } => {
            Ok(SerialFlashRoute::Katapult { serial_device })
        }
        SelectedUsbBootloader::Stm32Dfu { sysfs_path } => Ok(SerialFlashRoute::Stm32Dfu {
            sysfs_path,
            target: target_from_kconfig(kconfig).map_err(SystemFlashError::Stm32Target)?,
        }),
        SelectedUsbBootloader::PicoBoot { sysfs_path } => {
            Ok(SerialFlashRoute::PicoBoot { sysfs_path })
        }
    }
}

/// Flashes one prepared artifact after the coordinator has stopped Klipper.
pub fn flash_prepared_system(
    prepared: &PreparedBuild,
    firmware: &[u8],
    options: SystemKatapultOptions,
) -> Result<FlashResult, SystemFlashError> {
    match prepared
        .transport
        .as_ref()
        .ok_or(SystemFlashError::MissingTransport)?
    {
        McuTransport::Serial { device } => {
            flash_serial_system(device, &prepared.request.kconfig, firmware, options)
        }
        McuTransport::Can { interface, uuid } => {
            flash_can_system(interface, *uuid, firmware, options)
        }
    }
}

fn flash_serial_system(
    running_device: &str,
    kconfig: &str,
    firmware: &[u8],
    options: SystemKatapultOptions,
) -> Result<FlashResult, SystemFlashError> {
    let observed = SystemSerialIo::request_and_observe_any_usb_bootloader(
        std::path::Path::new(running_device),
        options.bootloader_timeout,
        options.poll_interval,
    )
    .map_err(SystemFlashError::Bootloader)?;
    match serial_route(observed, kconfig)? {
        SerialFlashRoute::Katapult { serial_device } => {
            let io = SystemSerialIo::open(&serial_device, options.baud_rate, options.read_timeout)
                .map_err(SystemFlashError::KatapultOpen)?;
            let mut backend = KatapultBackend::new(KatapultSerialTransport::new(io));
            backend
                .flash(firmware)
                .map_err(SystemFlashError::KatapultFlash)
        }
        SerialFlashRoute::Stm32Dfu { sysfs_path, target } => flash_stm32_at_path(
            crate::flash::stm32_dfu::Stm32DfuDevice::ROM_BOOTLOADER,
            &sysfs_path,
            target,
            firmware,
        )
        .map_err(SystemFlashError::Stm32Dfu),
        SerialFlashRoute::PicoBoot { sysfs_path } => {
            flash_picoboot_at_path(&sysfs_path, firmware).map_err(SystemFlashError::PicoBoot)
        }
    }
}

fn flash_can_system(
    interface: &str,
    uuid: u64,
    firmware: &[u8],
    options: SystemKatapultOptions,
) -> Result<FlashResult, SystemFlashError> {
    let io =
        SocketCanIo::open(interface, options.read_timeout).map_err(SystemFlashError::CanSocket)?;
    let bootstrap = request_can_bootloader(io, uuid).map_err(SystemFlashError::Can)?;
    thread::sleep(options.can_bootloader_settle);
    let mut backend = bootstrap.connect().map_err(SystemFlashError::Can)?;
    backend.flash(firmware).map_err(SystemFlashError::CanFlash)
}
