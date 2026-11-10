M-Flasher
=========

Back up, restore and reinstall the firmware of an M-VAVE FM-1 or SMK-37
Pro / Elite, and install m-bootloader.

1. Extract this zip and run M-Flasher.exe.
   The first time, Windows may say "Windows protected your PC": choose
   "More info", then "Run anyway".
   If Microsoft Defender deletes M-Flasher.exe or says it found a threat,
   that is a false alarm on a new, unsigned program: see "Microsoft
   Defender" at the end.
2. Put the unit in download mode and connect it with a USB data cable:
   - with m-bootloader: hold REC (SMK-37: SEQ REC) while switching on;
   - Felucca, SLOOP, Snoop37: hold OCT- and OCT+ for 5 s while it runs.
   When M-Flasher finds it, Windows asks to let M-Flasher make changes:
   it needs administrator rights to send the unit commands.
3. Start with "Back up the whole flash". Backups go to
   Documents\M-Flasher\backups.

Microsoft Defender
------------------
New versions are reported to Microsoft as false alarms when they are
released, which usually takes a few days to reach Defender. Until then:

- The safest: wait a day or two, update Defender (Windows Update) and
  download the zip again.
- Or, if the zip came from
  https://github.com/jvitkauskas/m-flasher/releases, open Windows
  Security > Virus & threat protection > Protection history and pick the
  M-Flasher entry:
  - "Threat quarantined": Actions > Restore. Defender finds it again: in
    the new "Threat found - action needed" entry choose Allow on device.
  - "Threat blocked": Actions > Allow, then download the zip again.

Please tell us the name Defender gave the threat (for example
"Trojan:Win32/Wacatac.B!ml") in an issue, so it can be reported.

M-Flasher is free. To support it:
https://ko-fi.com/jvitkauskas or https://github.com/sponsors/jvitkauskas

More: https://github.com/jvitkauskas/m-flasher
