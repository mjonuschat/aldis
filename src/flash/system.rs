//! Topology-scoped native flashing dispatch for prepared MCU updates.

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::flash::FlashBackend;
use crate::flash::FlashResult;
use crate::flash::bossa::{
    BossaError, BossaTarget, flash_system as flash_bossa,
    target_from_kconfig as bossa_target_from_kconfig,
};
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
    /// BOSSA-compatible SAM-BA serial bootloader and its Kconfig-derived offset.
    Bossa {
        /// BOSSA serial device at the observed topology.
        serial_device: PathBuf,
        /// Application placement derived from the prepared Kconfig.
        target: BossaTarget,
    },
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

/// Settings for topology-scoped native flashing backends.
#[derive(Clone, Debug)]
pub struct SystemFlashOptions {
    /// Timing and baud settings for Katapult.
    pub katapult: SystemKatapultOptions,
    /// `bossac` executable used for BOSSA-compatible SAMD bootloaders.
    pub bossac_program: PathBuf,
}

/// A visible phase of a native bootloader transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemFlashProgress {
    /// The running firmware is being asked to enter its bootloader.
    EnteringBootloader,
    /// The expected bootloader has re-enumerated at the selected topology.
    BootloaderReady,
    /// The firmware transfer is beginning.
    Flashing,
}

/// A selected update cannot be dispatched to a native backend.
#[derive(Debug)]
pub enum SystemFlashError {
    /// The prepared target did not retain a configured transport.
    MissingTransport,
    /// USB bootloader entry or topology observation failed.
    Bootloader(UsbBootloaderError),
    /// The re-enumerated USB device was not accessible before the timeout.
    UsbAccess(io::Error),
    /// The observed USB identity has no safe native backend.
    Selection(UsbBootloaderSelectionError),
    /// The observed STM32 route has no valid Kconfig application address.
    Stm32Target(Stm32DfuError),
    /// The observed BOSSA route has no valid Kconfig application offset.
    BossaTarget(BossaError),
    /// The Katapult USB serial device could not be opened.
    KatapultOpen(serialport::Error),
    /// Katapult rejected or could not verify its USB serial transfer.
    KatapultFlash(KatapultFlashError),
    /// STM32 ROM DfuSe flashing failed.
    Stm32Dfu(Stm32DfuError),
    /// PicoBoot flashing failed.
    PicoBoot(PicoBootError),
    /// BOSSA flashing failed.
    Bossa(BossaError),
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
    let bossa_target = bossa_target_from_kconfig(kconfig);
    match select_usb_bootloader(observed.clone()) {
        Ok(SelectedUsbBootloader::Katapult { serial_device, .. }) => {
            Ok(SerialFlashRoute::Katapult { serial_device })
        }
        Ok(SelectedUsbBootloader::Bossa { serial_device, .. }) => Ok(SerialFlashRoute::Bossa {
            serial_device,
            target: bossa_target.map_err(SystemFlashError::BossaTarget)?,
        }),
        Ok(SelectedUsbBootloader::Stm32Dfu { sysfs_path }) => Ok(SerialFlashRoute::Stm32Dfu {
            sysfs_path,
            target: target_from_kconfig(kconfig).map_err(SystemFlashError::Stm32Target)?,
        }),
        Ok(SelectedUsbBootloader::PicoBoot { sysfs_path }) => {
            Ok(SerialFlashRoute::PicoBoot { sysfs_path })
        }
        Err(UsbBootloaderSelectionError::Unsupported { .. }) => {
            let target = bossa_target.map_err(|_| {
                SystemFlashError::Selection(UsbBootloaderSelectionError::Unsupported {
                    usb_id: observed.usb_id.clone(),
                    manufacturer: observed.manufacturer.clone(),
                })
            })?;
            let serial_device = observed.serial_device.ok_or_else(|| {
                SystemFlashError::Selection(UsbBootloaderSelectionError::BossaSerialDeviceMissing(
                    observed.sysfs_path.clone(),
                ))
            })?;
            Ok(SerialFlashRoute::Bossa {
                serial_device,
                target,
            })
        }
        Err(error) => Err(SystemFlashError::Selection(error)),
    }
}

/// Flashes one prepared artifact after the coordinator has stopped Klipper.
pub fn flash_prepared_system(
    prepared: &PreparedBuild,
    firmware: &[u8],
    options: SystemFlashOptions,
) -> Result<FlashResult, SystemFlashError> {
    flash_prepared_system_with_progress(prepared, firmware, options, |_| {})
}

/// Flashes one prepared artifact while reporting native bootloader phases.
pub fn flash_prepared_system_with_progress(
    prepared: &PreparedBuild,
    firmware: &[u8],
    options: SystemFlashOptions,
    mut progress: impl FnMut(SystemFlashProgress),
) -> Result<FlashResult, SystemFlashError> {
    match prepared
        .transport
        .as_ref()
        .ok_or(SystemFlashError::MissingTransport)?
    {
        McuTransport::Serial { device } => flash_serial_system(
            device,
            &prepared.request.kconfig,
            firmware,
            options,
            &mut progress,
        ),
        McuTransport::Can { interface, uuid } => {
            flash_can_system(interface, *uuid, firmware, options, &mut progress)
        }
    }
}

fn flash_serial_system(
    running_device: &str,
    kconfig: &str,
    firmware: &[u8],
    options: SystemFlashOptions,
    progress: &mut impl FnMut(SystemFlashProgress),
) -> Result<FlashResult, SystemFlashError> {
    progress(SystemFlashProgress::EnteringBootloader);
    let observed = SystemSerialIo::request_and_observe_any_usb_bootloader(
        std::path::Path::new(running_device),
        options.katapult.bootloader_timeout,
        options.katapult.poll_interval,
    )
    .map_err(SystemFlashError::Bootloader)?;
    progress(SystemFlashProgress::BootloaderReady);
    wait_for_usb_access(
        &observed.sysfs_path,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
    .map_err(SystemFlashError::UsbAccess)?;
    progress(SystemFlashProgress::Flashing);
    match serial_route(observed, kconfig)? {
        SerialFlashRoute::Katapult { serial_device } => {
            let io = SystemSerialIo::open(
                &serial_device,
                options.katapult.baud_rate,
                options.katapult.read_timeout,
            )
            .map_err(SystemFlashError::KatapultOpen)?;
            let mut backend = KatapultBackend::new(KatapultSerialTransport::new(io));
            backend
                .flash(firmware)
                .map_err(SystemFlashError::KatapultFlash)
        }
        SerialFlashRoute::Bossa {
            serial_device,
            target,
        } => flash_bossa(&options.bossac_program, &serial_device, target, firmware)
            .map_err(SystemFlashError::Bossa),
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

fn wait_for_usb_access(
    sysfs_path: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> io::Result<()> {
    let device_node = usb_device_node(sysfs_path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_node)
        {
            Ok(_) => return Ok(()),
            Err(_error) if Instant::now() < deadline => thread::sleep(poll_interval),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{} was not readable and writable within {timeout:?}: {error}",
                        device_node.display()
                    ),
                ));
            }
        }
    }
}

fn usb_device_node(sysfs_path: &Path) -> io::Result<PathBuf> {
    let component = |name| {
        std::fs::read_to_string(sysfs_path.join(name))?
            .trim()
            .parse::<u16>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    };
    let bus = component("busnum")?;
    let device = component("devnum")?;
    Ok(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{device:03}")))
}

fn flash_can_system(
    interface: &str,
    uuid: u64,
    firmware: &[u8],
    options: SystemFlashOptions,
    progress: &mut impl FnMut(SystemFlashProgress),
) -> Result<FlashResult, SystemFlashError> {
    progress(SystemFlashProgress::EnteringBootloader);
    let io = SocketCanIo::open(interface, options.katapult.read_timeout)
        .map_err(SystemFlashError::CanSocket)?;
    let bootstrap = request_can_bootloader(io, uuid).map_err(SystemFlashError::Can)?;
    thread::sleep(options.katapult.can_bootloader_settle);
    let mut backend = bootstrap.connect().map_err(SystemFlashError::Can)?;
    progress(SystemFlashProgress::BootloaderReady);
    progress(SystemFlashProgress::Flashing);
    backend.flash(firmware).map_err(SystemFlashError::CanFlash)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::usb_device_node;

    #[test]
    fn locates_the_usb_device_node_from_its_sysfs_numbers() {
        let root = unique_temporary_path();
        fs::create_dir_all(&root).expect("create sysfs fixture");
        fs::write(root.join("busnum"), "1\n").expect("write bus number");
        fs::write(root.join("devnum"), "120\n").expect("write device number");

        assert_eq!(
            usb_device_node(&root).expect("derive device node"),
            PathBuf::from("/dev/bus/usb/001/120")
        );

        fs::remove_dir_all(root).expect("remove sysfs fixture");
    }

    fn unique_temporary_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mcu-update-usb-node-{}-{nonce}",
            std::process::id()
        ))
    }
}
