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
conversion is needed.

Backups go to `Documents/M-Flasher/backups` in your home folder; the
**Backups** link at the top of the window opens it.

## The flash map

The unit's 1 MiB flash is drawn as its 256 sectors of 4 KiB, coloured by what
they hold: bootloader, firmware, settings, Bluetooth pairings, factory
calibration, presets, sounds. When an action is chosen, the sectors it would
change are outlined; while writing, they light up as they are written and
verified. Hover over a sector to see its address and contents.

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
m-flasher run-ram IMAGE --addr 0x01C28000 [--call FC60]... [--read ADDR:LEN -o OUT]
```

Options: `--device PATH` (with more than one unit), `--board fm1|smk37` (when
the model is uncertain), `--backup-dir DIR`, `--yes`. Changes are shown first
and need a typed `yes`, or `--yes`.

- Windows: run `M-Flasher.exe` from an administrator prompt.
- macOS: `/Applications/M-Flasher.app/Contents/MacOS/M-Flasher info`, from
  Terminal; it asks for the password the same way, or run it with `sudo`. Not
  over SSH: macOS asks about the removable volume only on the screen.
- Linux: `m-flasher info` (from the tarball: `./m-flasher info`); `man m-flasher` with the packages.

## Running your own program in RAM

`m-flasher run-ram` is for developers: it uploads a program of your own to the
chip's RAM through download mode, starts it, and then talks to it. It is how
m-bootloader's flash loader is started, without M-Flasher's own loader and
without any flash operation.

```sh
m-flasher run-ram prog.bin --addr 0x01C28000 --call FC60 --call FC61 \
                  --read 0x01C28000:4096 -o ram.bin
```

- `IMAGE` is a flat binary linked for `--addr`, with its entry at the first
  byte, built like the flash loader (m-bootloader's `docs/SPEC.md` §10 gives
  the contract). `--addr` is hex and must be in `0x01C20000` to `0x01C2FFFF`,
  the flash loader's two 32 KiB slots, the only RAM known to be safe; the
  image must end before `0x01C30000`. It is refused otherwise, and also when it
  would overwrite an M-Flasher loader that is running (use the other slot).
- `--call OPCODE` (16-bit hex, repeatable) sends the 16-byte command (the
  opcode, then `FF`s) and prints the program's 16-byte answer in hex; the
  program is expected to echo the opcode in the first two bytes. Flash erase
  and write, memory write and jump (`FB00`, `FB01`, `FB04`, `FB06`, `FB08`) are
  never sent.
- `--read ADDR:LEN -o OUT` reads `LEN` bytes (decimal or `0x` hex, at most 16
  MiB) from `ADDR` (hex) into `OUT`, with `FD61` commands of at most 4096
  bytes: `FD 61`, the address (big-endian u32), the length (u16), then `FF`s.
  The program has to implement `FD61`, sending the bytes with the ROM's send
  callback.
- `--device PATH` as for the other commands.

This **runs arbitrary code on the chip**: only run programs you trust and
understand. It does not write to the flash itself, but a program of yours can
do anything the chip can. Nothing is kept: **switching the unit off and on
again returns it to normal**. It works whether or not M-Flasher's loader was
started before, and does not start it.

## Slow USB ports

A full read takes about 1.2 s and a firmware install about 10 s. Some USB
controllers are much slower with the unit (all ports of AMD's 600-series
chipset controller, for example: about 4.5 s for a read). The log shows the
read speed and suggests another port when it is slow; a port at the back,
next to the video outputs, is often on a faster controller.
