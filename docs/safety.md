# Safety

What M-Flasher checks, so that a write cannot leave the unit worse off than a
backup can repair.

**Before writing**

- The flash is read when the unit connects and checked against a CRC32 the
  chip computes itself. If they differ, nothing is written.
- The model is identified from the flash: the SMK-37's factory calibration,
  m-bootloader's board tag, the firmware's layout, the flash chip. If
  it is not certain, the app says so and asks; a package or bootloader for the
  other model is refused.
- A firmware package must be for the connected model, and its firmware must
  decrypt with this unit's chip key. The same goes for a package the stock
  bootloader is put back from, as its boot area replaces the unit's.
- Installing m-bootloader on a flash chip it has not been tested with brings a
  warning.
- Installing m-bootloader is refused if the unit's chip key is damaged: it is
  the one part of the boot configuration kept, and firmware installers need it.
- A full backup is saved before the first write of a session.

**While writing**

- Only the 4 KiB sectors that change are written, and only inside the area
  the action allows: installing firmware never touches the boot area unless
  asked, installing a bootloader writes nothing else.
- The unit's factory data (calibration, Bluetooth address) is kept, also when
  restoring another unit's backup.

**After writing**

- Everything written is checked: by the chip's CRC32 of it, or read back and
  compared. A mismatch is written once more; if it still differs, the app
  stops and says so.

**What it cannot fix for you**

- Firmwares keep their settings and data in different places. When the
  installed firmware's layout differs from the package's (Baud Girl, for
  example, keeps its settings inside the stock presets area), the app warns
  that what is left behind can confuse the new firmware. Restoring a backup
  made while a firmware was installed is the clean way back to it.
- The synth sounds (USR), which official updates never write, come back only
  from a backup. FM-1 016 overwrites them once it runs, so going from 016 back to
  015 leaves garbage preset names and wrong pitch, whichever bootloader is
  installed, until a backup with the original area is restored. Baud Girl also
  overwrites them, so stock firmware installed after it on an SMK-37 has no
  presets until then.
