# M-Flasher for the web

A static page that does what can be done from a browser, over USB MIDI: back up a unit, restore a backup, install firmware, and install m-bootloader. It needs a browser with Web MIDI, served over HTTPS or from `localhost`, and nothing else: no build step, no server, and nothing leaves the device.

```sh
python3 -m http.server 8000 --directory web   # then open http://localhost:8000
node --test web/test/*.test.mjs               # FLASHER_TEST_DATA=<dir with .fwsc files> runs more
```

## How it works

A browser can't reach the chip's download mode, which appears as a USB disk. It can reach MIDI, and most firmware can't read its flash over MIDI, so the page works through one of two programs of ours:

- **MIDI mode**, in m-bootloader 2.2 or newer, from its menu. It writes nothing until asked, and works with no firmware at all.
- **The stage** (`stage-*.fwsc`), for a unit running firmware. The firmware stores it through its own update route, about 8 KiB of what it keeps for updates, and restarts. The installed bootloader, stock or ours, runs it from RAM. It's the same code as MIDI mode. Felucca-family firmware only stores its own kind of updater, so the stage carries that kind's mark, and there's a second package for it that lists a blank `flash.bin`, which M-VAVE's firmware would refuse. It retires its update request first, so the next start returns to the firmware, and it gives up after 5 minutes with no message from the page.

Both answer the commands in [m-bootloader's specification](https://github.com/jvitkauskas/m-bootloader/blob/main/docs/SPEC.md), sections 11.6 and 11.7: `0x12` says what's answering, `0x23` reads raw flash, `0x24` is the chip's CRC-32 of a range. Writing goes through their existing install, from a package: the page builds one from a backup or a plan. It checks the whole package before erasing, erases the first firmware sector first and writes it last, so an interruption can't pass as firmware.

`lib/formats.js` and `lib/unit.js` follow M-Flasher's `src/formats.rs`, `src/boards.rs` and `src/ops.rs`.

## What it won't do

- Write the boot area or the key area from a backup, or the SMK-37's Bluetooth and calibration sectors. MIDI mode can't. m-bootloader is installed only through its own package, on firmware that accepts it.
- Send firmware's other service commands. On stock SMK-37 firmware, command `0x21` erases a sector with no protection, and the checksum doesn't cover the command byte. `lib/protocol.js` can only send what's in its `ALLOWED` list, and packages.
- Help a stock unit that won't start. That needs M-Flasher on a computer, or the USB_KEY dongle.

## Tested

See the pull request for the record. In short, on an FM-1 and an SMK-37 Pro, in Chromium through Web MIDI, every result compared byte for byte with M-Flasher on a computer. On the FM-1, the stage runs on stock 015 and 016, Baud Girl, Felucca, SLOOP 2.8.5, ChoralRoot and Melodee. On the SMK-37, it runs on stock Pro 016, Elite 016 (cross-flashed to a Pro), Baud Girl and ChoralRoot.

It can't start on ChanceTheMaker's SMK firmware (`SMK-37_9NN`), whose updater only takes its own packages, nor on the older `SMK-37 Pro_900`. There, use MIDI mode or M-Flasher.
