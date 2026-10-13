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

Download the latest build for your host's architecture (`aarch64`, `armv7l` for 32-bit Raspberry Pi OS, or `x86_64`) and extract it to `~/aldis`:

```
mkdir -p ~/aldis
curl -fL "https://github.com/mjonuschat/aldis/releases/latest/download/aldis-$(uname -m)-linux.tar.xz" \
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

## Agent (web UI integration)

```
sudo ~/aldis/aldis setup --agent
```

Installs and starts the `aldis` service under the sudo user, so it survives reboots and keeps running. Adds it to Moonraker's service list (`moonraker.asvc`) so web UIs can manage and restart it from their service menus. Restart Moonraker once afterwards so it picks up the new entry.

```
sudo ~/aldis/aldis setup --agent --remove
```

Stops and removes the service and drops it from Moonraker's service list.

### Several printers

One agent can serve every Klipper/Moonraker pair on the host. List the Moonrakers in a TOML file:

```toml
moonraker = ["http://127.0.0.1:7125", "http://127.0.0.1:7126"]
```

Pass the file with `--config PATH`, or repeat `--moonraker URL`. Flags add to the file. To install the service for two printers:

```
sudo ~/aldis/aldis setup --agent --moonraker http://127.0.0.1:7125 --moonraker http://127.0.0.1:7126
```

Each printer has its own MCU status, firmware updates and history. Updates run one at a time across the host.

Klipper services other than `klipper` (such as `klipper-2` or `klipper@voron`) need sudo 1.9.10 or newer: Debian 12, Raspberry Pi OS Bookworm, Ubuntu 24.04. The default `klipper` service works with any sudo. With several printers configured, `setup --check` prints the sudo version.

Flashing a USB-CAN bridge resets its CAN bus, so aldis refuses while another printer on that bus is printing, starting up, or cannot be checked. It stops nothing on the other printers, and their Klipper may need a `FIRMWARE_RESTART` once the bridge is back. `update` and `flash` run the same check when given `--config PATH` (or `ALDIS_CONFIG`). Without it they look only at the printer named by `--moonraker`.

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

`self-update` compares the running binary's version against the latest GitHub release and, unless already current, downloads and installs the matching platform archive over the current binary. No `sudo` is required as long as `~/aldis` is writable by the current user. If the agent service is running, `self-update` restarts it onto the new binary; otherwise it leaves the service alone.

`status` also reports MCUs Klipper configured but could not reach, with their connection info and why. `update` and `flash` refuse to run against a remote Moonraker or one whose Klipper service name is not a valid Klipper unit. Only one `update`, `flash`, or `reboot` runs at a time on the host, and a second one fails immediately instead of racing the first.

## Supported Flash Backends

These bootloaders are entered and flashed unattended, directly from the running Klipper application:

- **Katapult**, over USB serial, a plain UART (such as a Pi's GPIO UART), or CAN
- **STM32 ROM DFU** (`0483:df11`)
- **PicoBoot** (RP2040/RP2350, `2e8a:0003`/`2e8a:000f`)
- **SAM-BA** on SAMD21 (the Seeed XIAO SAMD21's UF2 bootloader, `2886:002f`)

Every other bootloader identity is reported as unsupported rather than guessed at. MCUs whose
firmware configuration names a family none of these bootloaders run on (Katapult builds only for
STM32, RP2040/RP235x, and LPC176x) are reported as unsupported up front and never offered an
update; SAMD51 and other non-SAMD21 ATSAMD boards are the common case.

A SAMD21 board must have been built with a bootloader offset and, for UF2 bootloaders such as the
XIAO's, with the matching `Bootloader type` (Kalico) so that the 1200-baud request lands in the
bootloader rather than rebooting into the application.

Only the XIAO's bootloader USB ID is recognised. Other SAMD21 boards are reported as an
unsupported bootloader until their ID is added and the flash is tested on that board.

### Katapult over a UART

When an MCU's `serial:` is a plain tty with no USB device behind it (for example `/dev/ttyAMA5`),
`aldis` sends Klipper's serial bootloader request, waits for Katapult to answer on the same tty, and
flashes only after it has. Requirements:

- Katapult must be built for the same UART and baud rate as the running firmware. `aldis` reads the
  baud rate from the firmware's embedded configuration (`CONFIG_SERIAL_BAUD`, 250000 when absent) and
  cannot read Katapult's own setting. If they differ, `aldis` stops with "the MCU did not enter
  Katapult" before sending any flash command, leaving the MCU waiting in Katapult.
- The host user needs access to the tty (typically the `dialout` group), and the UART must be enabled
  on the host and not used as a console.
- `aldis reboot` finds stuck bootloaders by scanning USB, so a Katapult left waiting on a UART is not
  listed. Run `flash` against the MCU again to recover it.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
