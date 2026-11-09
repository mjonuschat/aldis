# aldis

A tool for updating MCU firmware. `aldis` is named after the Aldis lamp, the signal lamp ships use to flash messages to each other.

<img src="images/demo.gif" alt="aldis update --pull --all --force" width="800">

## What It Does

`aldis` needs no per-MCU configuration. It discovers every supported MCU configured in a running Klipper/Moonraker installation, compares each one's running firmware with the local Klipper source checkout, and builds and flashes the outdated ones. It stops Klipper only when a build or flash is about to happen, and it reports its plan before changing anything.

## Requirements

- Linux (uses udev and sysfs directly; the build fails on other platforms)
- A running Moonraker instance
- Each target MCU must already be running a version new enough to report `mcu_kconfig`:
  - Klipper: `v0.13.0-753-g8c29c0a8e` or newer
  - Kalico: `v2026.09.00-3-g6127720c4` or newer
  - `aldis` reports MCUs running older firmware as unsupported legacy and never offers to update them. Flash them once by hand (for example with Klipper's `make flash`) to bring them under `aldis`.

## Installation

Download the latest build for your host's architecture (`aarch64`, `armv7l` for 32-bit Raspberry Pi OS, or `x86_64`) and extract it to `~/aldis`:

```
mkdir -p ~/aldis
curl -fL "https://github.com/mjonuschat/aldis/releases/latest/download/aldis-$(uname -m)-linux.tar.xz" \
  | tar xJ -C ~/aldis --strip-components=1
chmod +x ~/aldis/aldis
```

The rest of this document assumes this installation: `aldis` is not on `PATH` and runs as `~/aldis/aldis`.

## Setup

MCU updates need host permissions that a normal user lacks. Udev rules make USB bootloader devices accessible. A narrow sudoers policy lets `aldis` stop and start Klipper and install the Linux host MCU without a password prompt mid-update. Install both with:

```
sudo ~/aldis/aldis setup
```

Run `~/aldis/aldis setup --check` to verify that they are in place without changing anything.

## Agent (web UI integration)

```
sudo ~/aldis/aldis setup --agent
```

This installs and starts the `aldis` service under the sudo user, so it survives reboots. It also adds the service to Moonraker's service list (`moonraker.asvc`), so web UIs can manage and restart it from their service menus. Restart Moonraker once afterwards to pick up the new entry.

```
sudo ~/aldis/aldis setup --agent --remove
```

This stops and removes the service and drops it from Moonraker's service list.

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

The same file tells the agent where to find Beacon and Cartographer firmware outside the default places:

```toml
beacon_source = "/home/pi/beacon_klipper"
cartographer_source = "/home/pi/cartographer_firmware"
```

Klipper services other than `klipper` (such as `klipper-2` or `klipper@voron`) need sudo 1.9.10 or newer: Debian 12, Raspberry Pi OS Bookworm, Ubuntu 24.04. The default `klipper` service works with any sudo. With several printers configured, `setup --check` prints the sudo version.

Flashing a USB-CAN bridge resets its CAN bus, so aldis refuses while another printer on that bus is printing, starting up, or cannot be checked. It stops nothing on the other printers, but their Klipper may need a `FIRMWARE_RESTART` once the bridge is back. `update` and `flash` run the same check when given `--config PATH` (or `ALDIS_CONFIG`). Without it they look only at the printer named by `--moonraker`.

### HTTPS and TLS

The agent works with an `https://` Moonraker. It checks the certificate against the public web roots. A Moonraker with a self-signed certificate or a private CA needs one of these in the configuration file:

```toml
ca_file = "/etc/ssl/moonraker.pem"   # PEM certificates to trust instead of the public roots
insecure_skip_verify = true          # accept any certificate; use only on a network you trust
```

Set one or the other, not both. With `insecure_skip_verify` the connection is encrypted but the server is not authenticated, so anyone on the network path can impersonate Moonraker and the agent logs a warning at startup. Both settings apply to the agent only. `status`, `update` and the other commands verify an `https://` URL against the public roots.

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

- `--force` alone updates every eligible MCU, like `--all`. With specific `<mcu>` names, it force-updates only those, even when they are already current. `update` never replaces firmware with an older build or image, even with `--force`.
- `--pull` also combines with specific `<mcu>` names.
- `--auto` implies `--pull` and cannot be combined with `--pull`, `--force`, `--all`, or explicit MCU names.

Each build writes the MCU's embedded Kconfig to `.config` in the Klipper checkout and replaces whatever `make menuconfig` left there. Keep a copy of a hand-maintained `.config`. The run's workspace keeps the configuration as `make olddefconfig` expanded it.

`self-update` compares the running binary's version with the latest GitHub release. Unless the binary is current, it downloads the matching platform archive and installs it over the current binary. The archive's SHA-256 digest must match the release's `SHA256SUMS` file, or nothing is installed. `sudo` is not required when the current user can write to `~/aldis`. If the agent service is running, `self-update` restarts it onto the new binary; otherwise it leaves the service alone.

`status` also reports MCUs that Klipper configured but could not reach, with their connection info and the reason. `update` and `flash` refuse to run against a remote Moonraker or one whose Klipper service name is not a valid Klipper unit. Only one `update`, `flash`, or `reboot` runs at a time on the host, and a second one fails immediately rather than race the first.

## Supported Flash Backends

These bootloaders are entered and flashed unattended, directly from the running Klipper application:

- **Katapult**, over USB serial, a plain UART (such as a Pi's GPIO UART), or CAN
- **STM32 ROM DFU** (`0483:df11`)
- **PicoBoot** (RP2040/RP2350, `2e8a:0003`/`2e8a:000f`)
- **SAM-BA** on SAMD21 (the Seeed XIAO SAMD21's UF2 bootloader, `2886:002f`)
- **Bondtech INDX** toolboard (`04d8:e483`)

`aldis` reports every other bootloader identity as unsupported rather than guessing. It also
reports an MCU as unsupported up front, and never offers it an update, when its firmware
configuration names a family that none of these bootloaders run on (Katapult builds only for STM32,
RP2040/RP235x, and LPC176x). SAMD51 and other non-SAMD21 ATSAMD boards, the INDX excepted, are the
common case.

Build a SAMD21 board with a bootloader offset. For UF2 bootloaders such as the XIAO's, also select
the matching `Bootloader type` (Kalico), so the 1200-baud request lands in the bootloader instead
of rebooting into the application.

`aldis` recognises only the XIAO's bootloader USB ID. It reports other SAMD21 boards as having an
unsupported bootloader until their ID is added and the flash is tested on that board.

### Katapult over a UART

When an MCU's `serial:` is a plain tty with no USB device behind it (for example `/dev/ttyAMA5`),
`aldis` sends Klipper's serial bootloader request, waits for Katapult to answer on the same tty, and
flashes only after it has. Requirements:

- Katapult must be built for the same UART and baud rate as the running firmware. `aldis` reads the
  baud rate from the firmware's embedded configuration (`CONFIG_SERIAL_BAUD`, 250000 when absent) and
  cannot read Katapult's own setting. If the two differ, `aldis` stops with "the MCU did not enter
  Katapult" before sending any flash command and leaves the MCU waiting in Katapult.
- The host user needs access to the tty (typically the `dialout` group), and the UART must be enabled
  on the host and not used as a console.
- `aldis reboot` finds stuck bootloaders by scanning USB, so a Katapult left waiting on a UART is not
  listed. Run `flash` against the MCU again to recover it.

## Third-party firmware

Besides Klipper and Kalico, `aldis` updates:

- [FOCI](https://github.com/foci-rs/foci)
- [Beacon](https://github.com/beacon3d/beacon_klipper)
- [Cartographer](https://github.com/Cartographer3D/cartographer_firmware)

## License

GPL-3.0-only. See [LICENSE](LICENSE).
