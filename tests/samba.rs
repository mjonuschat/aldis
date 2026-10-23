use std::collections::VecDeque;
use std::io::{self, Read, Write};

use aldis::flash::FlashPort;
use aldis::flash::samba::{
    SambaAdapter, SambaError, SambaPort, SambaTarget, crc16_xmodem, reboot_into_application,
    target_from_kconfig,
};

const SAMD21_DEVICE_ID: u32 = 0x1001_0305;
const FLASH_PAGES: u32 = 4096;
const PAGE_SIZE_64_BYTES: u32 = 3 << 16;
const STAGING_ADDRESS: u32 = 0x2000_4000;
const APPLICATION_START: u32 = 0x2000;

struct FakeMonitor {
    command: Vec<u8>,
    payload_remaining: usize,
    outbox: VecDeque<u8>,
    staged: Vec<u8>,
    copy_source: u32,
    flash: Vec<u8>,
    device_id: u32,
    corrupt_first_write: bool,
    copy_ack: &'static [u8],
    reset_requested: bool,
    commands: Vec<String>,
}

impl FakeMonitor {
    fn new() -> Self {
        Self {
            command: Vec::new(),
            payload_remaining: 0,
            outbox: VecDeque::new(),
            staged: Vec::new(),
            copy_source: 0,
            flash: vec![0xff; 0x40000],
            device_id: SAMD21_DEVICE_ID,
            corrupt_first_write: false,
            copy_ack: b"Y\n\r",
            reset_requested: false,
            commands: Vec::new(),
        }
    }

    fn hex(text: &str) -> u32 {
        u32::from_str_radix(text, 16).unwrap()
    }

    fn execute(&mut self, text: &str) {
        self.commands.push(text.to_owned());
        let (kind, arguments) = text.split_at(1);
        let arguments = arguments.trim_end_matches('#');
        match kind {
            "N" => {}
            "V" => self
                .outbox
                .extend(b"v1.1 [Arduino:XYZ] Nov 27 2019 16:35:59\n\r\0"),
            "w" => {
                let (address, length) = arguments.split_once(',').unwrap();
                assert_eq!(length, "4");
                let word = match Self::hex(address) {
                    0x4100_2018 => self.device_id,
                    0x4100_4008 => FLASH_PAGES | PAGE_SIZE_64_BYTES,
                    other => panic!("unexpected word read at {other:#x}"),
                };
                self.outbox.extend(word.to_le_bytes());
            }
            "W" => {
                let (address, value) = arguments.split_once(',').unwrap();
                assert_eq!(Self::hex(address), 0xE000_ED0C);
                assert_eq!(Self::hex(value), 0x05FA_0004);
                self.reset_requested = true;
            }
            "S" => {
                let (address, length) = arguments.split_once(',').unwrap();
                assert_eq!(Self::hex(address), STAGING_ADDRESS);
                self.payload_remaining = Self::hex(length) as usize;
                self.staged.clear();
            }
            "Y" => {
                let (address, length) = arguments.split_once(',').unwrap();
                let (address, length) = (Self::hex(address), Self::hex(length) as usize);
                if length == 0 {
                    self.copy_source = address;
                } else {
                    assert_eq!(self.copy_source, STAGING_ADDRESS);
                    let start = address as usize;
                    self.flash[start..start + length].copy_from_slice(&self.staged[..length]);
                    if self.corrupt_first_write {
                        self.flash[start] ^= 0xff;
                        self.corrupt_first_write = false;
                    }
                }
                self.outbox.extend(self.copy_ack);
            }
            "Z" => {
                let (address, length) = arguments.split_once(',').unwrap();
                let (address, length) = (Self::hex(address) as usize, Self::hex(length) as usize);
                let crc = crc16_xmodem(&self.flash[address..address + length]);
                self.outbox.extend(format!("Z{crc:08X}#\n\r").into_bytes());
            }
            other => panic!("unexpected command {other}"),
        }
    }
}

impl Write for FakeMonitor {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for byte in bytes {
            if self.payload_remaining > 0 {
                self.staged.push(*byte);
                self.payload_remaining -= 1;
            } else {
                self.command.push(*byte);
                if *byte == b'#' {
                    let text = String::from_utf8(std::mem::take(&mut self.command)).unwrap();
                    self.execute(&text);
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for FakeMonitor {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.outbox.is_empty() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let count = buffer.len().min(self.outbox.len());
        for slot in &mut buffer[..count] {
            *slot = self.outbox.pop_front().unwrap();
        }
        Ok(count)
    }
}

impl SambaPort for FakeMonitor {
    fn discard_input(&mut self) -> io::Result<()> {
        self.outbox.clear();
        Ok(())
    }
}

fn target() -> SambaTarget {
    SambaTarget {
        application_start: APPLICATION_START,
    }
}

fn firmware(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index * 7 + 3) as u8).collect()
}

#[test]
fn writes_verifies_and_resets_an_image_spanning_several_chunks() {
    let image = firmware(0x2400);
    let mut adapter = SambaAdapter::new(FakeMonitor::new(), target());

    let result = adapter.flash(&image).unwrap();

    assert_eq!(result.padded_bytes, 0x3000);
    assert_eq!(result.reported_pages, None);
    let monitor = adapter.into_port();
    let start = APPLICATION_START as usize;
    assert_eq!(&monitor.flash[start..start + image.len()], &image[..]);
    assert!(
        monitor.flash[start + image.len()..start + 0x3000]
            .iter()
            .all(|byte| *byte == 0xff)
    );
    assert!(monitor.reset_requested);
    assert_eq!(monitor.commands.last().unwrap(), "WE000ED0C,05FA0004#");
}

#[test]
fn refuses_a_chip_that_is_not_a_samd21() {
    let mut monitor = FakeMonitor::new();
    monitor.device_id = 0x6006_0000;
    let mut adapter = SambaAdapter::new(monitor, target());

    assert!(matches!(
        adapter.flash(&firmware(64)),
        Err(SambaError::UnsupportedChip(0x6006_0000))
    ));
    assert!(adapter.into_port().flash.iter().all(|byte| *byte == 0xff));
}

#[test]
fn refuses_an_image_that_does_not_fit_in_flash() {
    let mut adapter = SambaAdapter::new(FakeMonitor::new(), target());

    let error = adapter.flash(&firmware(0x40000)).unwrap_err();

    assert!(matches!(
        error,
        SambaError::FirmwareTooLarge {
            end: 0x42000,
            flash_size: 0x40000
        }
    ));
    assert!(adapter.into_port().flash.iter().all(|byte| *byte == 0xff));
}

#[test]
fn refuses_an_empty_image() {
    let mut adapter = SambaAdapter::new(FakeMonitor::new(), target());

    assert!(matches!(adapter.flash(&[]), Err(SambaError::EmptyFirmware)));
}

#[test]
fn reports_the_block_whose_checksum_differs_and_does_not_reset() {
    let mut monitor = FakeMonitor::new();
    monitor.corrupt_first_write = true;
    let mut adapter = SambaAdapter::new(monitor, target());

    let error = adapter.flash(&firmware(0x1800)).unwrap_err();

    assert!(matches!(
        error,
        SambaError::VerificationMismatch { address: 0x2000 }
    ));
    assert!(!adapter.into_port().reset_requested);
}

#[test]
fn rejects_an_unexpected_copy_acknowledgement() {
    let mut monitor = FakeMonitor::new();
    monitor.copy_ack = b"E\n\r";
    let mut adapter = SambaAdapter::new(monitor, target());

    let error = adapter.flash(&firmware(64)).unwrap_err();

    assert!(matches!(
        error,
        SambaError::UnexpectedReply { command: 'Y', ref reply } if reply == b"E\n\r"
    ));
}

#[test]
fn reboots_into_the_application_without_touching_flash() {
    let mut monitor = FakeMonitor::new();
    reboot_into_application(&mut monitor).unwrap();

    assert!(monitor.reset_requested);
    assert!(monitor.flash.iter().all(|byte| *byte == 0xff));
    assert!(
        !monitor
            .commands
            .iter()
            .any(|command| command.starts_with('Y'))
    );
}

#[test]
fn derives_the_application_address_from_the_bootloader_offset() {
    assert_eq!(
        target_from_kconfig("CONFIG_MACH_ATSAMD=y\nCONFIG_MACH_SAMD21G18=y\n")
            .unwrap()
            .application_start,
        0x2000
    );
    assert_eq!(
        target_from_kconfig("CONFIG_SAMD_FLASH_START_4000=y\n")
            .unwrap()
            .application_start,
        0x4000
    );
    assert!(matches!(
        target_from_kconfig("CONFIG_SAMD_FLASH_START_0000=y\n"),
        Err(SambaError::NoBootloaderOffset)
    ));
}

#[test]
fn computes_the_xmodem_crc_the_monitor_reports() {
    assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
}
