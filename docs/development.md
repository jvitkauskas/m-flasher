# How it works

The JieLi AC791N's mask ROM has a USB download mode (UBOOT1.00): the chip
shows up as a USB mass-storage device that takes vendor SCSI commands to
write its RAM and jump into it. M-Flasher writes a flash loader into RAM and
starts it; the loader then reads, erases and writes the SPI flash. The loader
comes from [m-bootloader](https://github.com/jvitkauskas/m-bootloader), which
also specifies its commands. If another tool's loader is already running,
M-Flasher asks for the unit to be switched off and on. The protocol follows
[jl-uboot-tool](https://github.com/kagaimiq/jl-uboot-tool) by kagaimiq.

The loader takes the flash's size from its ID once, when it starts, and without
one refuses every read. M-Flasher reuses a loader that is already running, so
it checks for that with a small read, and starts a new one in the other slot
once before giving up with the ID the chip now gives.

How the unit is reached differs by system:

- **Windows:** SCSI pass-through to the disk (`SCSI_PASS_THROUGH_DIRECT`),
  which needs administrator rights.
- **Linux:** the SCSI generic driver (`SG_IO`), or libusb with
  `--features usb`.
- **macOS:** it does not pass such commands through, so the app takes the
  device from the disk driver and speaks USB mass storage (Bulk-Only
  Transport) itself, through libusb. That needs root: the window runs as the
  user and starts a small root helper through the system's password dialog
  (`src/macos.rs`), which does the USB transfers for it.

A unit that is running normally is switched to download mode with the soft
key, the SysEx `F0 22 24 35 7D F7` on its first MIDI port, which M-VAVE's
firmware and Felucca answer by restarting into the ROM's download mode. Linux
sends it through the kernel's sequencer interface (`/dev/snd/seq`), which
shares the port with whatever else uses it (PipeWire's MIDI bridge, browsers,
DAWs), with libc only, so the program still needs no ALSA library. Without a
sequencer it writes to the raw-MIDI device (port 0, as `amidi -p hw:N,0,0`),
which another program may hold: then it says so. Windows and macOS use
[midir](https://github.com/Boddlnagg/midir) (WinMM, CoreMIDI).

It is one native executable: the window (egui), the command line, the
flash loader, the m-bootloader builds and the fonts are all inside it.

## Source

| | |
| --- | --- |
| `src/formats.rs` | flash and package formats: CRCs, ENC, file tables, boot area, `.fwsc` |
| `src/boards.rs` | the models and how a flash image identifies them |
| `src/sectormap.rs` | the flash map: a region for each of the 256 sectors |
| `src/scsi.rs` | SCSI pass-through: Linux `SG_IO`, Windows `SCSI_PASS_THROUGH_DIRECT` |
| `src/usb.rs` | USB mass storage through libusb (macOS; Linux with `--features usb`) |
| `src/macos.rs` | macOS: the root helper, started through the system's password dialog |
| `src/jl.rs` | the download-mode protocol and starting the loader |
| `src/midi.rs` | the soft key: download mode over USB MIDI |
| `src/ops.rs` | the actions and their safety rules |
| `src/controller.rs` | the window's logic, in background threads |
| `src/ui.rs` | the window: the panel and the flash map |
| `src/wordmark.rs` | the striped "M-FLASHER" lettering |
| `src/cli.rs` | the command line, including `run-ram` |
| `data/` | the bundled flash loader and m-bootloader builds, from its [releases](https://github.com/jvitkauskas/m-bootloader/releases) ([NOTICE](../data/NOTICE.md)) |
| `assets/` | fonts and the window icon ([README](../assets/README.md)) |
| `linux/`, `windows/`, `macos/` | each system's package: icons, instructions, the udev rule, the disk image |
| `linux/package/` | the `.deb` and `.rpm`: nFPM's configuration, the launcher, AppStream metadata, the man page |

## Building

```sh
cargo build --release          # target/release/m-flasher
cargo test --release           # FLASHER_TEST_DATA=DIR adds the tests with real flash images
```

The optional test data includes `fm1-016.fwsc`, the vendor's
[FM-1 016 package](https://yms-file-store.oss-cn-hongkong.aliyuncs.com/software/firmware/FM-1.fwsc)
(SHA-256 `d017fb65e70aca97ee29d944470c3515a74581df5c0576bf70de917c7b2b61d5`).
The URL may change with later releases. The tests check its three app files
(`app.bin`, `cfg_tool.bin`, `DX7.bin`), upgrades and downgrades, settings wipes,
and stock/m-bootloader replacement against simulated NOR flash. Synthetic
tests cover the new layout and boot configuration placement without this file.

FM-1 016's image ends at `0xA9000`; VM is at `0xBA000..0xDA000`, SEQ64 at
`0xDA000..0xEC000`, and BTIF at `0xEC000..0xED000`. Its larger stock bootloader
moves `isd_config.ini` from `0x38D0` to `0x3938`. Bootloader replacement reads
the configuration's address and length from the file table, retaining its
bytes and moving it within the boot area only when needed to fit a larger
bootloader. It refuses to move it over other data.

GitHub Actions (`.github/workflows/build.yml`) builds and tests the Linux,
Windows and macOS packages for every push, and publishes them as a release
for a `v*` tag; the `.deb` and `.rpm` are installed and run in clean
containers of the distributions they are for (`linux/package/test-install.sh`).
`linux/package/build.sh` makes them with [nFPM](https://nfpm.goreleaser.com).
On a Mac, `macos/package.sh` makes the app and the disk image,
and `macos/render.sh` renders the icons and the disk image's background from
their SVGs.

## Tested

FM-1s sold from late 2026 have a UCUN UC25HQ80 flash (`0xB36014`) in place of the
Puya P25Q80H (`0x856014`). Owners report backups and restores with M-Flasher on
it, but m-bootloader has not been tested on it, and one unit did not start after
installing it ([#13](https://github.com/jvitkauskas/m-flasher/issues/13)).

m-bootloader 1.3 and flash loader 1.3, on an SMK-37 Pro from Linux: installing
it, booting its firmware and stock 016, swapping with the older loader in both
directions, and installing stock firmware and restoring, ending byte-identical.
On an FM-1, the build just before it boots with both the 015 and 016 boot
configurations. Not yet tried on Windows or macOS, or with M-UPGRADE.

m-bootloader 1.4 does not read the boot configuration, so installing it keeps
only the 34-byte chip-key record, at `0x3FDC`, and it may use the room up to
there. The ROM loads a full-size image on an FM-1 and an SMK-37. Build
`1.4-ac26c70`, on the same two units: installing with M-Flasher and M-UPGRADE gives
the same boot area from every starting configuration, stock firmware runs, and
putting stock back and Baud Girl's installer both restore the full configuration.
On the FM-1, official 015 and 016 updates restore it too, and SLOOP, Felucca,
ChoralRoot and Melodee install without touching the boot area. The SMK-37's own
official update has not been run from it: its firmware 900 refuses at `FAILED -5`,
and from stock 016 M-UPGRADE needs force upgrade. The stock bootloader reads the
keys, so it goes back with its package's whole boot area. Older M-Flasher versions
keep the unit's configuration when putting it back, so do not use them for that
on such a unit.

On an SMK-37 Pro, from Linux, Windows and macOS (an M3 Mac): identifying
stock firmware, Baud Girl and Snoop37; backups (identical to jl-uboot-tool's
reads); installing m-bootloader; installing stock firmware over Baud
Girl and over Snoop37; restoring a backup (the flash afterwards identical to
it). On an FM-1, from Linux: backups (two reads identical), installing m-bootloader and putting
the stock bootloader back, restoring, and the unit's screen during operations.

Download mode over USB MIDI, from Linux: on an SMK-37 Pro with the stock
bootloader and firmware 016 (switched, then read and checked), and on an
FM-1. The soft key itself was seen to work, with `amidi`, on the stock
firmware of the FM-1 (015), the SMK-37 Pro and the SMK-37 Elite (016).
Windows and macOS not yet tried.

FM-1 016 compatibility has been checked with the vendor package and simulated
flash, and installing 016 with M-Flasher has been tested on a unit.

Fame MiniFM support comes from comparing its
[`MiniFM_016` package](https://images.musicstore.de/assets/downloads/fame/Fame-Mini-FM/MiniFM.zip)
(`MiniFM_016.fwsc`, SHA-256
`f10b297b3f0e1789110ba1832b12865b7fc39d5d6e0e7c9a375461339268ca77`) with FM-1 016:
the same chip key, region addresses and boot area, and the soft key handled by
the same code. Cross-flashing has been tested with both packages against
simulated flash only, **not yet on a MiniFM**, and the soft key on its firmware
015 is unconfirmed. The optional test data can include it as `minifm-016.fwsc`.
