# aldis

A tool for updating MCU firmware. `aldis` is named after the Aldis lamp,
the signal lamp ships use to flash messages to each other.

## What It Does

`aldis` needs no per-MCU configuration: it discovers every supported MCU
already configured in a running Klipper/Moonraker installation, compares
each one's running firmware against the local Klipper source checkout, and
builds and flashes the ones that are out of date. It stops Klipper only
when a build or flash is about to happen, and it reports its plan before
changing anything.

## Requirements

- Linux (uses udev and sysfs directly; the build fails on other platforms)
- A running Moonraker instance
- Each target MCU must already be running a version new enough to report
  `mcu_kconfig`:
  - Klipper: `v0.13.0-753-g8c29c0a8e` or newer
  - Kalico: `v2026.09.00-3-g6127720c4` or newer
  - MCUs running older firmware are reported as unsupported legacy and
    `aldis` will never offer to update them; flash them one last time by
    hand (e.g. Klipper's own `make flash`) to bring them under `aldis`.

## Installation

Download the latest build for your platform (most Klipper hosts are
Raspberry Pi, i.e. `aarch64-linux`; use `x86_64-linux` on a typical PC) and
extract it to `~/aldis`:

```
mkdir -p ~/aldis
curl -L https://github.com/mjonuschat/aldis/releases/latest/download/aldis-aarch64-linux.tar.xz \
  | tar xJ -C ~/aldis --strip-components=1
chmod +x ~/aldis/aldis
```

The rest of this document assumes `aldis` was installed this way, i.e. is
not on `PATH` and is invoked as `~/aldis/aldis`.

## Setup

MCU updates need host permissions that a normal user does not have by
default: udev rules so USB bootloader devices are accessible, and a narrow
sudoers policy so Klipper can be stopped and started without a password
prompt mid-update.

```
sudo ~/aldis/aldis setup
```

installs both. Run `~/aldis/aldis setup --check` to verify they're in
place without changing anything.

## Usage

```
~/aldis/aldis status                    # discovered MCUs and whether firmware is current
~/aldis/aldis inspect                   # MCU configuration reported by Moonraker
~/aldis/aldis update --all              # build and flash every eligible MCU
~/aldis/aldis update <mcu> [<mcu> ...]  # build and flash specific MCUs
~/aldis/aldis update --auto             # download Klipper/Kalico updates, then build and flash everything outdated, no prompts
~/aldis/aldis update --all --force      # update even MCUs already on the checkout revision
```

`update` requires host permissions installed by `~/aldis/aldis setup` (see
above). `--pull` and `--force` also work with specific `<mcu>` targets;
`--auto` already implies both and cannot be combined with them.

## Supported Flash Backends

These bootloaders are entered and flashed unattended, directly from the
running Klipper application:

- **Katapult**, over USB serial or CAN
- **STM32 ROM DFU** (`0483:df11`)
- **PicoBoot** (RP2040/RP2350, `2e8a:0003`/`2e8a:000f`)

Every other bootloader identity is reported as unsupported rather than
guessed at.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
