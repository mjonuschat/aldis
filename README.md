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

Download the latest build for your host's architecture (`aarch64` or `x86_64`) and extract it to `~/aldis`:

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

Installs and starts the `aldis` service under the sudo user, so it survives reboots and keeps running. Adds it to Moonraker's service list (`moonraker.asvc`) so Fluidd and Mainsail can manage and restart it from their service menus.

```
sudo ~/aldis/aldis setup --agent --remove
```

Stops and removes the service and drops it from Moonraker's service list.

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

`status` also reports MCUs Klipper configured but could not reach, with their connection info and why. `update` and `flash` refuse to run against a remote Moonraker instance or a non-default Klipper service, and only one `update`, `flash`, or `reboot` runs at a time — a second concurrent run fails immediately rather than racing the first.

## Fluidd

With the agent installed, Fluidd shows a **Settings → Firmware Updates** card listing every MCU
with its running firmware version and an Update button where the agent offers one. Install the
agent with

```
sudo ~/aldis/aldis setup --agent
sudo systemctl restart moonraker
```

The Moonraker restart is needed once so Moonraker picks up the `aldis` service that setup adds
to `moonraker.asvc`; until then Fluidd only shows the card while the agent is connected, and
the Services list in Fluidd's Host menu cannot start or stop it.

Progress is shown in the same dialog Fluidd uses for software updates. Turn on "Enable
notifications" on the card to be told when an MCU falls behind the running Klipper host.

## Mainsail

A Mainsail build with firmware update support shows a **Machine → Firmware Updates** panel
below the Update Manager, listing every MCU with its running firmware version and an Update
button where the agent offers one. Setup is the same as for Fluidd: install the agent with
`sudo ~/aldis/aldis setup --agent` and restart Moonraker once.

Mainsail asks for confirmation before each update, like its software updates, unless "Hide
update warnings" is enabled in its UI settings. Progress is shown in Mainsail's update dialog.
When an MCU falls behind the running Klipper host, the notification bell shows an entry that can
be dismissed until the next reboot or until Klipper is next updated.

## Supported Flash Backends

These bootloaders are entered and flashed unattended, directly from the running Klipper application:

- **Katapult**, over USB serial or CAN
- **STM32 ROM DFU** (`0483:df11`)
- **PicoBoot** (RP2040/RP2350, `2e8a:0003`/`2e8a:000f`)

Every other bootloader identity is reported as unsupported rather than guessed at. MCUs whose
firmware configuration names a family none of these bootloaders run on (Katapult builds only for
STM32, RP2040/RP235x, and LPC176x) are reported as unsupported up front and never offered an
update; SAMD21 boards are the common case.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
