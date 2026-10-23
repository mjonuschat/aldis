//! SAMD21 flashing through the SAM-BA monitor that Arduino-style and UF2 bootloaders expose over
//! USB CDC.
//!
//! The command set is the monitor's ASCII protocol: `w`/`W` read and write a word, `S` stages raw
//! bytes in RAM, `Y` copies staged RAM into flash (erasing as it goes) and `Z` returns a CRC-16 of
//! a memory range.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use super::{FlashPort, FlashResult};

const DSU_DEVICE_ID: u32 = 0x4100_2018;
const NVMCTRL_PARAM: u32 = 0x4100_4008;
const NVM_USER_ROW: u32 = 0x0080_4000;
const AIRCR: u32 = 0xE000_ED0C;
const AIRCR_SYSRESETREQ: u32 = 0x05FA_0004;

const SAMD21_DEVICE_ID_MASK: u32 = 0xFFFF_0000;
const SAMD21_DEVICE_ID: u32 = 0x1001_0000;

const STAGING_ADDRESS: u32 = 0x2000_4000;
const CHUNK_SIZE: usize = 0x1000;

const COPY_ACK: &[u8; 3] = b"Y\n\r";
const CRC_REPLY_LEN: usize = 12;
const MAX_VERSION_LINE: usize = 128;

/// Where the application lives in flash, derived from the embedded Kconfig.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SambaTarget {
    /// Flash address the application image is written to.
    pub application_start: u32,
}

/// Derives the SAMD21 application address from Klipper's embedded Kconfig.
///
/// The embedded Kconfig is a defconfig, so the 8KiB bootloader offset, being the SAMD21 default,
/// is the absence of any other offset symbol.
pub fn target_from_kconfig(kconfig: &str) -> Result<SambaTarget, SambaError> {
    let enabled = |symbol: &str| kconfig.lines().any(|line| line.trim() == symbol);
    if enabled("CONFIG_SAMD_FLASH_START_0000=y") {
        return Err(SambaError::NoBootloaderOffset);
    }
    if enabled("CONFIG_MACH_SAMD21E15=y") {
        return Err(SambaError::InsufficientRam);
    }
    let application_start = if enabled("CONFIG_SAMD_FLASH_START_4000=y") {
        0x4000
    } else {
        0x2000
    };
    Ok(SambaTarget { application_start })
}

/// A native SAM-BA transfer failure.
#[derive(Debug, thiserror::Error)]
pub enum SambaError {
    /// The firmware was built to start at flash address zero, where the bootloader lives.
    #[error(
        "the firmware is built without a bootloader offset, so it would overwrite the bootloader"
    )]
    NoBootloaderOffset,
    /// The part's RAM cannot hold the staging buffer the transfer uses.
    #[error("the SAMD21E15 has too little RAM to stage firmware through the SAM-BA bootloader")]
    InsufficientRam,
    /// The bootloader's write protection disagrees with the firmware's application address.
    #[error(
        "the firmware starts at {application_start:#x} but the bootloader protects the first {protected_size:#x} bytes of flash; rebuild with the matching bootloader offset"
    )]
    BootloaderSizeMismatch {
        /// The application address the firmware was built for.
        application_start: u32,
        /// The size the chip's BOOTPROT fuse protects.
        protected_size: u32,
    },
    /// Serial communication with the bootloader failed or timed out.
    #[error("serial communication with the SAM-BA bootloader failed: {0}")]
    Io(#[from] io::Error),
    /// The bootloader answered a command with something other than the expected reply.
    #[error("the SAM-BA bootloader replied {reply:?} to a {command:?} command")]
    UnexpectedReply {
        /// The command letter that was sent.
        command: char,
        /// The bytes received instead of the acknowledgement.
        reply: Vec<u8>,
    },
    /// The connected chip is not a SAMD21.
    #[error("the chip behind the SAM-BA bootloader (device ID {0:#010x}) is not a SAMD21")]
    UnsupportedChip(u32),
    /// The image is empty.
    #[error("the firmware image is empty")]
    EmptyFirmware,
    /// The image extends past the end of the chip's flash.
    #[error("the firmware needs flash up to {end:#x} but the chip's flash ends at {flash_size:#x}")]
    FirmwareTooLarge {
        /// First address past the padded image.
        end: u64,
        /// Flash size reported by the NVM controller.
        flash_size: u64,
    },
    /// The flash contents did not match the image after writing.
    #[error("flash verification failed for the block at {address:#x}")]
    VerificationMismatch {
        /// Start address of the mismatching block.
        address: u32,
    },
}

/// A byte stream to a SAM-BA monitor.
pub trait SambaPort: Read + Write {
    /// Drops any unread bytes.
    fn discard_input(&mut self) -> io::Result<()>;
}

impl<T: SambaPort + ?Sized> SambaPort for &mut T {
    fn discard_input(&mut self) -> io::Result<()> {
        (**self).discard_input()
    }
}

/// A serial device opened for SAM-BA at an explicit path.
pub struct SystemSambaPort {
    port: Box<dyn serialport::SerialPort>,
}

impl SystemSambaPort {
    /// Opens the bootloader's CDC device with a bounded read timeout.
    pub fn open(path: &Path, read_timeout: Duration) -> serialport::Result<Self> {
        let port = serialport::new(path.to_string_lossy(), 115_200)
            .timeout(read_timeout)
            .open()?;
        Ok(Self { port })
    }
}

impl Read for SystemSambaPort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.port.read(buffer)
    }
}

impl Write for SystemSambaPort {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.port.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
    }
}

impl SambaPort for SystemSambaPort {
    fn discard_input(&mut self) -> io::Result<()> {
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }
}

/// Resets a SAMD21 sitting in its bootloader, which starts the application already in flash.
pub fn reboot_into_application(port: impl SambaPort) -> Result<(), SambaError> {
    let mut monitor = Monitor { port };
    monitor.handshake()?;
    monitor.reset()
}

struct Monitor<P> {
    port: P,
}

impl<P: SambaPort> Monitor<P> {
    fn handshake(&mut self) -> Result<(), SambaError> {
        self.port.write_all(b"N#")?;
        self.port.flush()?;
        self.port.discard_input()?;
        self.port.write_all(b"V#")?;
        self.port.flush()?;

        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        while byte[0] != b'\n' {
            self.port.read_exact(&mut byte)?;
            line.push(byte[0]);
            if line.len() > MAX_VERSION_LINE {
                return Err(SambaError::UnexpectedReply {
                    command: 'V',
                    reply: line,
                });
            }
        }
        tracing::debug!(version = %String::from_utf8_lossy(&line).trim(), "sam-ba monitor version");

        // The banner ends "\n\r" and some monitors append a NUL; neither is guaranteed.
        for _ in 0..2 {
            match self.port.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::TimedOut => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, SambaError> {
        self.port
            .write_all(format!("w{address:08X},4#").as_bytes())?;
        let mut word = [0_u8; 4];
        self.port.read_exact(&mut word)?;
        Ok(u32::from_le_bytes(word))
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), SambaError> {
        self.port
            .write_all(format!("W{address:08X},{value:08X}#").as_bytes())?;
        Ok(())
    }

    fn stage(&mut self, bytes: &[u8]) -> Result<(), SambaError> {
        self.port
            .write_all(format!("S{STAGING_ADDRESS:08X},{:08X}#", bytes.len()).as_bytes())?;
        self.port.write_all(bytes)?;
        Ok(())
    }

    fn expect_copy_ack(&mut self) -> Result<(), SambaError> {
        let mut reply = [0_u8; 3];
        self.port.read_exact(&mut reply)?;
        if &reply == COPY_ACK {
            Ok(())
        } else {
            Err(SambaError::UnexpectedReply {
                command: 'Y',
                reply: reply.to_vec(),
            })
        }
    }

    fn write_flash(&mut self, address: u32, length: usize) -> Result<(), SambaError> {
        self.port
            .write_all(format!("Y{STAGING_ADDRESS:08X},0#").as_bytes())?;
        self.expect_copy_ack()?;
        self.port
            .write_all(format!("Y{address:08X},{length:08X}#").as_bytes())?;
        self.expect_copy_ack()
    }

    fn checksum(&mut self, address: u32, length: usize) -> Result<u16, SambaError> {
        self.port
            .write_all(format!("Z{address:08X},{length:08X}#").as_bytes())?;
        let mut reply = [0_u8; CRC_REPLY_LEN];
        self.port.read_exact(&mut reply)?;
        std::str::from_utf8(&reply[1..9])
            .ok()
            .filter(|_| reply[0] == b'Z' && reply[9] == b'#')
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .map(|crc| crc as u16)
            .ok_or_else(|| SambaError::UnexpectedReply {
                command: 'Z',
                reply: reply.to_vec(),
            })
    }

    fn flash_size(&mut self) -> Result<u64, SambaError> {
        let param = self.read_word(NVMCTRL_PARAM)?;
        let pages = u64::from(param & 0xFFFF);
        let page_size = 8_u64 << ((param >> 16) & 0x7);
        Ok(pages * page_size)
    }

    fn protected_bootloader_size(&mut self) -> Result<Option<u32>, SambaError> {
        let bootprot = self.read_word(NVM_USER_ROW)? & 0x7;
        Ok((bootprot != 0x7).then(|| 0x8000 >> bootprot))
    }

    fn reset(&mut self) -> Result<(), SambaError> {
        self.write_word(AIRCR, AIRCR_SYSRESETREQ)?;
        self.port.flush()?;
        Ok(())
    }
}

/// Flashes a SAMD21 through its SAM-BA monitor.
pub struct SambaAdapter<P> {
    monitor: Monitor<P>,
    target: SambaTarget,
}

impl<P: SambaPort> SambaAdapter<P> {
    /// Binds an open monitor connection to the application address it should write.
    pub fn new(port: P, target: SambaTarget) -> Self {
        Self {
            monitor: Monitor { port },
            target,
        }
    }

    /// Returns the underlying connection.
    pub fn into_port(self) -> P {
        self.monitor.port
    }
}

impl<P: SambaPort> FlashPort for SambaAdapter<P> {
    type Error = SambaError;

    fn flash(&mut self, firmware: &[u8]) -> Result<FlashResult, Self::Error> {
        if firmware.is_empty() {
            return Err(SambaError::EmptyFirmware);
        }
        self.monitor.handshake()?;

        let device_id = self.monitor.read_word(DSU_DEVICE_ID)?;
        if device_id & SAMD21_DEVICE_ID_MASK != SAMD21_DEVICE_ID {
            return Err(SambaError::UnsupportedChip(device_id));
        }

        let start = self.target.application_start;
        if let Some(protected_size) = self.monitor.protected_bootloader_size()?
            && protected_size != start
        {
            return Err(SambaError::BootloaderSizeMismatch {
                application_start: start,
                protected_size,
            });
        }

        let padded_bytes = firmware.len().div_ceil(CHUNK_SIZE) * CHUNK_SIZE;
        let end = u64::from(start) + padded_bytes as u64;
        let flash_size = self.monitor.flash_size()?;
        if end > flash_size {
            return Err(SambaError::FirmwareTooLarge { end, flash_size });
        }

        for (index, chunk) in firmware.chunks(CHUNK_SIZE).enumerate() {
            let mut block = chunk.to_vec();
            block.resize(CHUNK_SIZE, 0xFF);
            self.monitor.stage(&block)?;
            self.monitor
                .write_flash(start + (index * CHUNK_SIZE) as u32, CHUNK_SIZE)?;
        }

        for (index, chunk) in firmware.chunks(CHUNK_SIZE).enumerate() {
            let address = start + (index * CHUNK_SIZE) as u32;
            if self.monitor.checksum(address, chunk.len())? != crc16_xmodem(chunk) {
                return Err(SambaError::VerificationMismatch { address });
            }
        }

        self.monitor.reset()?;
        Ok(FlashResult {
            reported_pages: None,
            padded_bytes,
        })
    }
}

/// CRC-16/XMODEM, the checksum the monitor's `Z` command computes.
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0_u16, |mut crc, byte| {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
        crc
    })
}
