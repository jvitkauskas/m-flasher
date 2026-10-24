# Using M-Flasher

## Installing

**Windows** (10 or 11). Extract `M-Flasher-…-windows.zip` and run
`M-Flasher.exe`. It asks for administrator rights, which it needs to talk to
the unit. The app is not signed, so the first time Windows may say it
protected your PC: choose **More info → Run anyway**.

**macOS** (11 or newer, Apple silicon and Intel). Open
`M-Flasher-…-macos.dmg` and drag M-Flasher to Applications. The first time
macOS blocks it, as it is not signed with an Apple certificate: **System
Settings → Privacy & Security → Open Anyway**. When the app finds the unit:

- it asks for your password in the system's dialog: only an administrator may
  take a USB device from macOS's disk driver;
- macOS asks once whether M-Flasher may access files on a removable volume
  (that is the unit): choose **Allow**.

Start the app soon after connecting the unit: left with macOS's disk driver,
the unit may leave download mode after about half a minute.

**Linux** (x86-64 or ARM64, glibc 2.35 or newer). The packages set up
everything: the program, the udev rule that lets you reach the unit without
root, the SCSI generic driver, and a launcher in the app menu.

- Debian 12+, Ubuntu 22.04+, Linux Mint, Raspberry Pi OS (64-bit):
  `sudo apt install ./m-flasher_…_amd64.deb` (`_arm64.deb` for ARM64)
- Fedora, RHEL / AlmaLinux / Rocky 10, openSUSE Tumbleweed and Leap 16:
  `sudo dnf install ./m-flasher-….x86_64.rpm`, or `sudo zypper install
  ./m-flasher-….x86_64.rpm` (`.aarch64.rpm` for ARM64)

Connect the unit after installing (again, if it was connected), and start
M-Flasher from the app menu, or `m-flasher` in a terminal, as yourself: under
sudo the window cannot reach the desktop. The packages do not update
themselves: install the new one from each release.

On other distributions, use `m-flasher-…-linux-x86_64.tar.gz` (or
`-aarch64`; `uname -m` tells which) and follow its `README.txt`: install the
udev rule once, connect the unit, and run `./m-flasher`. M-Flasher talks to
the unit through the SCSI generic driver: run `sudo modprobe sg` if there is
no `/dev/sg*`. `m-flasher.png` is the icon, for a launcher of your own.

## Download mode

The unit's chip has a USB download mode built into its ROM, which works even
when the firmware does not. To enter it:

- with the unit running normally and connected: M-Flasher finds it on USB
  MIDI and offers **DOWNLOAD MODE**, which restarts it into download mode
  with a MIDI message (`m-flasher download-mode` on the command line). This
  works with M-VAVE's own firmware (FM-1 015, SMK-37 Pro and Elite 016) and
  Felucca, whichever bootloader the unit has;
- with m-bootloader: hold **REC** (SMK-37: **SEQ REC**) while switching the
  unit on;
- with Felucca, SLOOP or Snoop37 running: hold **OCT-** and **OCT+** for 5 s;
- a unit whose firmware is damaged often ends up there by itself.

Connect it directly with a USB **data** cable, not through a hub. In download
mode the unit's screen stays as it was, and nothing appears in Explorer or
Finder: the system sees an empty disk ("WL82 UBOOT1.00"). M-Flasher finds it
by itself, reads the whole flash and shows the model, the firmware and the
bootloader; the unit's own screen then shows what is going on.

A unit with the stock bootloader and a working stock firmware cannot be put
in download mode from its keys: use **DOWNLOAD MODE**. If the firmware does
not start, the USB_KEY dongle (a Raspberry Pi Pico) still reaches download
mode.

When you are done, unplug the unit and switch it off and on again.

## The actions

| Action | What it writes |
| --- | --- |
| **Back up the whole flash** | nothing: saves the 1 MiB flash image and a `.json` describing it |
| **Restore a backup** | the parts that differ; a backup of another unit keeps this unit's own factory data (calibration, Bluetooth address) |
| **Install firmware (.fwsc)** | the firmware from an M-VAVE package (or a custom one in the same format); optionally the package's stock bootloader, and optionally erasing the settings and Bluetooth pairings |
| **Install m-bootloader** | the boot area only (16 KiB), the build for the identified model |
| **Put back the stock bootloader** | the boot area only, from an official package |

Use the official `.fwsc` files from M-VAVE's website as they are: no
conversion is needed. FM-1 **016** is supported, including its DX7 data file
and new settings and sequencer layout. Erasing settings clears settings and
Bluetooth pairings at the addresses declared by the package; saved sequences
are kept. When switching between 015 and 016, the app warns about the changed
data layout. Restore a backup to return to the earlier firmware with its data.

Backups go to `Documents/M-Flasher/backups` in your home folder; the
**Backups** link at the top of the window opens it.

## The flash map

The unit's 1 MiB flash is drawn as its 256 sectors of 4 KiB, coloured by what
they hold: bootloader, firmware, settings, Bluetooth pairings, factory
calibration, presets, sequences, sounds. When an action is chosen, the sectors
it would change are outlined; while writing, they light up as they are written
and verified. Hover over a sector to see its address and contents.

## Command line

The same executable is a command-line tool when given arguments:

```sh
m-flasher find                 # the units in download mode (and those running, on USB MIDI)
m-flasher download-mode        # switch a running unit to download mode over USB MIDI
m-flasher info                 # what is on the unit
m-flasher backup [DIR]
m-flasher restore BACKUP.bin
m-flasher firmware PACKAGE.fwsc [--with-bootloader] [--wipe-settings]
m-flasher bootloader [--stock PACKAGE.fwsc | --file UBOOT.boot]
```

Options: `--device PATH` (with more than one unit), `--board fm1|smk37` (when
the model is uncertain), `--backup-dir DIR`, `--yes`. Changes are shown first
and need a typed `yes`, or `--yes`.

- Windows: run `M-Flasher.exe` from an administrator prompt.
- macOS: `/Applications/M-Flasher.app/Contents/MacOS/M-Flasher info`, from
  Terminal; it asks for the password the same way, or run it with `sudo`. Not
  over SSH: macOS asks about the removable volume only on the screen.
- Linux: `m-flasher info` (from the tarball: `./m-flasher info`); `man m-flasher` with the packages.

## Slow USB ports

A full read takes about 1.2 s and a firmware install about 10 s. Some USB
controllers are much slower with the unit (all ports of AMD's 600-series
chipset controller, for example: about 4.5 s for a read). The log shows the
read speed and suggests another port when it is slow; a port at the back,
next to the video outputs, is often on a faster controller.
