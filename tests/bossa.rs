use std::path::Path;

use mcu_update::flash::bossa::{BossaError, BossaTarget, bossac_command, target_from_kconfig};

#[test]
fn derives_samd21_bossa_offset_from_embedded_kconfig() {
    let target = target_from_kconfig("CONFIG_SAMD_FLASH_START_2000=y\n")
        .expect("8KiB bootloader offset should parse");

    assert_eq!(target.offset, 0x2000);
}

#[test]
fn derives_samx5_bossa_offset_from_embedded_kconfig() {
    let target = target_from_kconfig("CONFIG_SAMD_FLASH_START_4000=y\n")
        .expect("16KiB bootloader offset should parse");

    assert_eq!(target.offset, 0x4000);
}

#[test]
fn rejects_missing_or_conflicting_bossa_offsets() {
    assert!(matches!(
        target_from_kconfig(""),
        Err(BossaError::InvalidFlashStartConfiguration)
    ));
    assert!(matches!(
        target_from_kconfig("CONFIG_SAMD_FLASH_START_2000=y\nCONFIG_SAMD_FLASH_START_4000=y\n"),
        Err(BossaError::InvalidFlashStartConfiguration)
    ));
}

#[test]
fn constructs_the_bossac_command_used_by_klipper() {
    let command = bossac_command(
        Path::new("/home/pi/klipper/lib/bossac/bin/bossac"),
        Path::new("/dev/ttyACM0"),
        BossaTarget { offset: 0x2000 },
        Path::new("/tmp/firmware.bin"),
    );

    assert_eq!(command.program, "/home/pi/klipper/lib/bossac/bin/bossac");
    assert_eq!(
        command.arguments,
        vec![
            "-U",
            "-p",
            "/dev/ttyACM0",
            "--offset=0x2000",
            "-b",
            "-R",
            "-w",
            "/tmp/firmware.bin",
            "-v",
        ]
    );
    assert_eq!(command.current_dir, None);
}
