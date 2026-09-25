# aldis

A tool for updating MCU firmware. `aldis` is named after the Aldis lamp, the signal lamp ships use to flash messages to each other.

<img src="images/demo.gif" alt="aldis update --pull --all --force" width="800">

## What It Does

`aldis` needs no per-MCU configuration: it discovers every supported MCU already configured in a running Klipper/Moonraker installation, compares each one's running firmware against the local Klipper source checkout, and builds and flashes the ones that are out of date. It stops Klipper only when a build or flash is about to happen, and it reports its plan before changing anything.

## Requirements

- Linux (uses udev and sysfs directly; the build fails on other platforms)
- A running Moonraker instance
- Each target MCU must already be running a version new enough to report `mcu_kconfig`:
  - Klipper: `v0.13.0-753-g8c29c0a8e` or newer
  - Kalico: `v2026.09.00-3-g6127720c4` or newer
  - MCUs running older firmware are reported as unsupported legacy and `aldis` will never offer to update them; flash them one last time by hand (e.g. Klipper's own `make flash`) to bring them under `aldis`.

## Installation

Download the latest build for your platform (most Klipper hosts are Raspberry Pi, i.e. `aarch64-linux`; use `x86_64-linux` on a typical PC) and extract it to `~/aldis`:

```
mkdir -p ~/aldis
curl -L https://github.com/mjonuschat/aldis/releases/latest/download/aldis-aarch64-linux.tar.xz \
  | tar xJ -C ~/aldis --strip-components=1
chmod +x ~/aldis/aldis
```

The rest of this document assumes `aldis` was installed this way, i.e. is not on `PATH` and is invoked as `~/aldis/aldis`.

## Setup

MCU updates need host permissions that a normal user does not have by default: udev rules so USB bootloader devices are accessible, and a narrow sudoers policy so Klipper can be stopped and started, and the Linux host MCU installed, without a password prompt mid-update.

```
sudo ~/aldis/aldis setup
```

installs both. Run `~/aldis/aldis setup --check` to verify they're in place without changing anything.

## Usage

```
~/aldis/aldis status                    # discovered MCUs and whether firmware is current
~/aldis/aldis inspect                   # MCU configuration reported by Moonraker
~/aldis/aldis update                    # build and flash outdated, supported MCUs (prompted per MCU)
~/aldis/aldis update <mcu> [<mcu> ...]  # build and flash specific MCUs
~/aldis/aldis update --all              # build and flash every supported MCU, even ones already current
~/aldis/aldis update --auto             # download Klipper/Kalico updates, then build and flash everything outdated, no prompts
~/aldis/aldis update --force            # update every eligible MCU, same as --all, but also works with specific <mcu> targets
~/aldis/aldis self-update               # check for and install a newer aldis release
~/aldis/aldis self-update --check       # report whether a newer release is available, without installing it
```

`update` requires the host permissions installed by `~/aldis/aldis setup` (see above). A few flags interact:

- `--force` alone updates every eligible MCU, same as `--all`. Combined with specific `<mcu>` names, it force-updates just those, even if already current.
- `--pull` can be combined with specific `<mcu>` names too.
- `--auto` already implies `--pull` and can't be combined with `--pull`, `--force`, `--all`, or explicit MCU names.

`self-update` compares the running binary's version against the latest GitHub release and, unless already current, downloads and installs the matching platform archive over the current binary. No `sudo` is required as long as `~/aldis` is writable by the current user.

## Supported Flash Backends

These bootloaders are entered and flashed unattended, directly from the running Klipper application:

- **Katapult**, over USB serial or CAN
- **STM32 ROM DFU** (`0483:df11`)
- **PicoBoot** (RP2040/RP2350, `2e8a:0003`/`2e8a:000f`)

Every other bootloader identity is reported as unsupported rather than guessed at.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
