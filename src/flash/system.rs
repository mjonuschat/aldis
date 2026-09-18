//! Topology-scoped native flashing dispatch for prepared MCU updates.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::flash::FlashPort;
use crate::flash::FlashResult;
use crate::flash::katapult::adapter::KatapultFlashError;
use crate::flash::katapult::bootstrap::{CanBootstrapError, request_can_bootloader};
use crate::flash::katapult::can::SocketCanIo;
use crate::flash::katapult::serial::{KatapultSerialTransport, SystemSerialIo, UsbBootloaderError};
use crate::flash::katapult::{adapter::KatapultAdapter, system::SystemKatapultOptions};
use crate::flash::picoboot::{PicoBootAdapter, PicoBootError};
use crate::flash::stm32_dfu::{
    Stm32DfuAdapter, Stm32DfuError, Stm32DfuTarget, target_from_kconfig,
};
use crate::flash::usb_bootloader::{
    ObservedUsbBootloader, SelectedUsbBootloader, UsbBootloaderSelectionError,
    select_usb_bootloader,
};
use crate::flash::usb_sysfs::usb_device_ancestor;
use crate::moonraker::McuTransport;
use crate::prepare::PreparedBuild;
use crate::retry::retry_until_available;

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

/// Settings for topology-scoped native flashing backends.
#[derive(Clone, Debug)]
pub struct SystemFlashOptions {
    /// Timing and baud settings for Katapult.
    pub katapult: SystemKatapultOptions,
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
    /// A USB-CAN bridge could not be related to its USB topology.
    CanUsbTopology(io::Error),
    /// Katapult CAN bootloader entry or node assignment failed.
    Can(CanBootstrapError<io::Error>),
    /// Katapult rejected or could not verify its CAN transfer.
    CanFlash(KatapultFlashError),
}

impl fmt::Display for SystemFlashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::flash::katapult::Command;
        use crate::flash::katapult::session::SessionError;

        match self {
            Self::CanFlash(KatapultFlashError::Session(SessionError::RetriesExhausted {
                command: Command::Connect,
                ..
            })) => write!(
                f,
                "the CAN bootloader did not respond to a connection request"
            ),
            Self::CanFlash(_) => write!(
                f,
                "the CAN bootloader rejected or could not verify the firmware transfer"
            ),
            Self::Can(_) => write!(f, "could not enter the bootloader over CAN"),
            Self::CanSocket(error) => {
                write!(f, "could not open the configured CAN interface: {error}")
            }
            Self::CanUsbTopology(error) => {
                write!(f, "could not identify the USB CAN adapter: {error}")
            }
            Self::Bootloader(_) => {
                write!(
                    f,
                    "the expected USB bootloader did not appear before the timeout"
                )
            }
            Self::UsbAccess(error) => {
                write!(f, "the USB bootloader was not accessible: {error}")
            }
            Self::KatapultOpen(error) => {
                write!(f, "could not open the Katapult bootloader: {error}")
            }
            Self::KatapultFlash(_) => write!(
                f,
                "the Katapult bootloader rejected or could not verify the firmware transfer"
            ),
            Self::Stm32Dfu(_) => write!(f, "STM32 DFU flashing failed"),
            Self::PicoBoot(_) => write!(f, "RP PicoBoot flashing failed"),
            Self::Stm32Target(_) => write!(
                f,
                "the STM32 firmware configuration has no valid flash address"
            ),
            Self::Selection(_) => write!(
                f,
                "the re-enumerated bootloader is not supported for automatic flashing"
            ),
            Self::MissingTransport => write!(f, "the MCU does not expose a configured transport"),
        }
    }
}

impl std::error::Error for SystemFlashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingTransport | Self::Can(_) => None,
            Self::Bootloader(error) => Some(error),
            Self::UsbAccess(error) => Some(error),
            Self::Selection(error) => Some(error),
            Self::Stm32Target(error) | Self::Stm32Dfu(error) => Some(error),
            Self::KatapultOpen(error) => Some(error),
            Self::KatapultFlash(error) | Self::CanFlash(error) => Some(error),
            Self::PicoBoot(error) => Some(error),
            Self::CanSocket(error) => Some(error),
            Self::CanUsbTopology(error) => Some(error),
        }
    }
}

/// Context for diagnosing an unsupported bootloader identity. Attached only
/// to the debug log; it never affects backend selection.
#[derive(Clone, Debug, Default)]
pub struct UnsupportedBootloaderContext {
    /// The MCU transport that was asked to enter its bootloader.
    pub transport: String,
    /// Wall-clock time between the reset request and the final observation.
    pub reset_elapsed: Option<Duration>,
}

/// Derives exactly one native USB route from one observed bootloader.
pub fn serial_route(
    observed: ObservedUsbBootloader,
    kconfig: &str,
    context: &UnsupportedBootloaderContext,
) -> Result<SerialFlashRoute, SystemFlashError> {
    match select_usb_bootloader(observed) {
        Ok(SelectedUsbBootloader::Katapult { serial_device, .. }) => {
            Ok(SerialFlashRoute::Katapult { serial_device })
        }
        Ok(SelectedUsbBootloader::Stm32Dfu { sysfs_path }) => Ok(SerialFlashRoute::Stm32Dfu {
            sysfs_path,
            target: target_from_kconfig(kconfig).map_err(SystemFlashError::Stm32Target)?,
        }),
        Ok(SelectedUsbBootloader::PicoBoot { sysfs_path }) => {
            Ok(SerialFlashRoute::PicoBoot { sysfs_path })
        }
        Err(error @ UsbBootloaderSelectionError::Unsupported { .. }) => {
            tracing::debug!(
                transport = %context.transport,
                machine = %kconfig_machine(kconfig),
                flash_offset_symbols = ?kconfig_flash_start_symbols(kconfig),
                reset_elapsed = ?context.reset_elapsed,
                "unsupported bootloader context"
            );
            Err(SystemFlashError::Selection(error))
        }
        Err(error) => Err(SystemFlashError::Selection(error)),
    }
}

fn kconfig_machine(kconfig: &str) -> String {
    kconfig
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("CONFIG_MACH_") && line.ends_with("=y"))
        .unwrap_or_default()
        .to_owned()
}

fn kconfig_flash_start_symbols(kconfig: &str) -> Vec<String> {
    kconfig
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("_FLASH_START_") && line.ends_with("=y"))
        .map(str::to_owned)
        .collect()
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
        McuTransport::Can { interface, uuid } => flash_can_system(
            interface,
            *uuid,
            &prepared.request.kconfig,
            firmware,
            options,
            &mut progress,
        ),
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
    let started = Instant::now();
    let observed = SystemSerialIo::request_and_observe_any_usb_bootloader(
        std::path::Path::new(running_device),
        options.katapult.bootloader_timeout,
        options.katapult.poll_interval,
    )
    .map_err(SystemFlashError::Bootloader)?;
    let context = UnsupportedBootloaderContext {
        transport: running_device.to_owned(),
        reset_elapsed: Some(started.elapsed()),
    };
    flash_observed_usb(observed, kconfig, firmware, options, progress, &context)
}

fn flash_observed_usb(
    observed: ObservedUsbBootloader,
    kconfig: &str,
    firmware: &[u8],
    options: SystemFlashOptions,
    progress: &mut impl FnMut(SystemFlashProgress),
    context: &UnsupportedBootloaderContext,
) -> Result<FlashResult, SystemFlashError> {
    progress(SystemFlashProgress::BootloaderReady);
    match serial_route(observed, kconfig, context)? {
        SerialFlashRoute::Katapult { serial_device } => {
            progress(SystemFlashProgress::Flashing);
            let io = open_katapult_serial_when_ready(
                &serial_device,
                options.katapult.baud_rate,
                options.katapult.read_timeout,
                options.katapult.bootloader_timeout,
                options.katapult.poll_interval,
            )
            .map_err(SystemFlashError::KatapultOpen)?;
            let mut backend = KatapultAdapter::new(KatapultSerialTransport::new(io));
            backend
                .flash(firmware)
                .map_err(SystemFlashError::KatapultFlash)
        }
        SerialFlashRoute::Stm32Dfu { sysfs_path, target } => {
            wait_for_usb_access(
                &sysfs_path,
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .map_err(SystemFlashError::UsbAccess)?;
            progress(SystemFlashProgress::Flashing);
            Stm32DfuAdapter::new(
                crate::flash::stm32_dfu::Stm32DfuDevice::ROM_BOOTLOADER,
                &sysfs_path,
                target,
            )
            .flash(firmware)
            .map_err(SystemFlashError::Stm32Dfu)
        }
        SerialFlashRoute::PicoBoot { sysfs_path } => {
            wait_for_usb_access(
                &sysfs_path,
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .map_err(SystemFlashError::UsbAccess)?;
            progress(SystemFlashProgress::Flashing);
            PicoBootAdapter::new(&sysfs_path)
                .flash(firmware)
                .map_err(SystemFlashError::PicoBoot)
        }
    }
}

fn open_katapult_serial_when_ready(
    serial_device: &Path,
    baud_rate: u32,
    read_timeout: Duration,
    timeout: Duration,
    poll_interval: Duration,
) -> serialport::Result<SystemSerialIo> {
    retry_until_available(timeout, poll_interval, || {
        SystemSerialIo::open(serial_device, baud_rate, read_timeout).inspect_err(|error| {
            tracing::debug!(error = %error, "katapult serial device not yet ready, still waiting");
        })
    })
}

fn wait_for_usb_access(
    sysfs_path: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> io::Result<()> {
    let device_node = usb_device_node(sysfs_path)?;
    wait_for_device_access(&device_node, timeout, poll_interval)
}

fn wait_for_device_access(
    device_node: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> io::Result<()> {
    retry_until_available(timeout, poll_interval, || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_node)
            .inspect_err(|error| {
                tracing::debug!(error = %error, "device not yet accessible, still waiting");
            })
    })
    .map(|_| ())
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{} was not readable and writable within {timeout:?}: {error}",
                device_node.display()
            ),
        )
    })
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
    kconfig: &str,
    firmware: &[u8],
    options: SystemFlashOptions,
    progress: &mut impl FnMut(SystemFlashProgress),
) -> Result<FlashResult, SystemFlashError> {
    progress(SystemFlashProgress::EnteringBootloader);
    let usb_bridge = usb_can_bridge(kconfig);
    let usb_path = usb_bridge
        .then(|| usb_device_path_for_can_interface(interface))
        .transpose()
        .map_err(SystemFlashError::CanUsbTopology)?;
    let initial_usb_identity = usb_path
        .as_deref()
        .map(usb_identity)
        .transpose()
        .map_err(SystemFlashError::CanUsbTopology)?;
    let io = SocketCanIo::open(interface, options.katapult.read_timeout)
        .map_err(SystemFlashError::CanSocket)?;
    let bootstrap = request_can_bootloader(io, uuid, options.katapult.read_timeout)
        .map_err(SystemFlashError::Can)?;
    if let (Some(usb_path), Some((usb_id, manufacturer))) = (usb_path, initial_usb_identity) {
        let started = Instant::now();
        let observed = SystemSerialIo::observe_any_usb_bootloader_at_path(
            &usb_path,
            &usb_id,
            &manufacturer,
            options.katapult.bootloader_timeout,
            options.katapult.poll_interval,
        )
        .map_err(SystemFlashError::Bootloader)?;
        let context = UnsupportedBootloaderContext {
            transport: format!("CAN {interface} (uuid {uuid:016x})"),
            reset_elapsed: Some(started.elapsed()),
        };
        return flash_observed_usb(observed, kconfig, firmware, options, progress, &context);
    }
    thread::sleep(options.katapult.can_bootloader_settle);
    let mut backend = bootstrap.connect().map_err(SystemFlashError::Can)?;
    progress(SystemFlashProgress::BootloaderReady);
    progress(SystemFlashProgress::Flashing);
    backend.flash(firmware).map_err(SystemFlashError::CanFlash)
}

fn usb_can_bridge(kconfig: &str) -> bool {
    kconfig
        .lines()
        .any(|line| line.trim() == "CONFIG_USBCANBUS=y")
}

fn usb_device_path_for_can_interface(interface: &str) -> io::Result<PathBuf> {
    let interface_path = std::fs::canonicalize(Path::new("/sys/class/net").join(interface))?;
    usb_device_ancestor(&interface_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("could not find a USB device for CAN interface {interface:?}"),
        )
    })
}

fn usb_identity(usb_path: &Path) -> io::Result<(String, String)> {
    let value = |name: &str| {
        std::fs::read_to_string(usb_path.join(name))
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default()
    };
    Ok((
        format!("{}:{}", value("idVendor"), value("idProduct")),
        value("manufacturer"),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        SystemFlashError, kconfig_flash_start_symbols, kconfig_machine, usb_can_bridge,
        usb_device_node,
    };
    use crate::flash::katapult::Command;
    use crate::flash::katapult::adapter::KatapultFlashError;
    use crate::flash::katapult::session::SessionError;

    #[test]
    fn recognizes_a_usb_can_bridge_from_embedded_kconfig() {
        assert!(usb_can_bridge("CONFIG_USBCANBUS=y\n"));
        assert!(!usb_can_bridge("# CONFIG_USBCANBUS is not set\n"));
    }

    #[test]
    fn explains_an_unresponsive_can_bootloader_without_a_debug_dump() {
        let detail = SystemFlashError::CanFlash(KatapultFlashError::Session(
            SessionError::RetriesExhausted {
                command: Command::Connect,
                last_failure: "transport error".to_owned(),
            },
        ))
        .to_string();

        assert_eq!(
            detail,
            "the CAN bootloader did not respond to a connection request"
        );
        assert!(!detail.contains("RetriesExhausted"));
    }

    #[test]
    fn finds_the_selected_machine_symbol_in_the_embedded_kconfig() {
        assert_eq!(
            kconfig_machine("# comment\nCONFIG_MACH_STM32H723=y\nCONFIG_OTHER=y\n"),
            "CONFIG_MACH_STM32H723=y"
        );
        assert_eq!(kconfig_machine("CONFIG_OTHER=y\n"), "");
    }

    #[test]
    fn finds_every_enabled_flash_start_symbol_in_the_embedded_kconfig() {
        assert_eq!(
            kconfig_flash_start_symbols(
                "CONFIG_STM32_FLASH_START_2000=y\n# CONFIG_SAMD_FLASH_START_2000 is not set\n"
            ),
            vec!["CONFIG_STM32_FLASH_START_2000=y".to_owned()]
        );
        assert!(kconfig_flash_start_symbols("CONFIG_OTHER=y\n").is_empty());
    }

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
        std::env::temp_dir().join(format!("aldis-usb-node-{}-{nonce}", std::process::id()))
    }
}
