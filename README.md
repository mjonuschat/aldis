# mcu-update

A tool for updating MCU firmware.

## Flash Support Notes

BOSSA/SAM-BA support is treated as a manual bootloader-entry path unless a
specific board has proven reliable software entry. For boards such as the
Seeed Studio XIAO SAMD21, the updater may build the Klipper artifact and flash
with `bossac` after the operator has manually placed the board in the BOSSA
bootloader, but it should not present that flow as an unattended in-place
update from the running Klipper application.

This puts BOSSA in the same operational bucket as SD-card or other manual
serial flashing paths: prepare the artifact, ask for explicit manual
bootloader entry, verify the expected bootloader identity, then flash.
