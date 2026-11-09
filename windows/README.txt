M-Flasher
=========

Back up, restore and reinstall the firmware of an M-VAVE FM-1 or SMK-37
Pro / Elite, and install m-bootloader.

1. Extract this zip and run M-Flasher.exe. It asks for administrator rights.
   The first time, Windows may say "Windows protected your PC": choose
   "More info", then "Run anyway".
   If Microsoft Defender deletes M-Flasher.exe or says it found a threat,
   that is a false alarm on a new, unsigned program: see "Microsoft
   Defender" at the end.
2. Put the unit in download mode and connect it with a USB data cable:
   - with m-bootloader: hold REC (SMK-37: SEQ REC) while switching on;
   - Felucca, SLOOP, Snoop37: hold OCT- and OCT+ for 5 s while it runs.
3. Start with "Back up the whole flash". Backups go to
   Documents\M-Flasher\backups.

Microsoft Defender
------------------
New versions are reported to Microsoft as false alarms when they are
released, which usually takes a few days to reach Defender. Until then:

- Check the zip came from https://github.com/jvitkauskas/m-flasher/releases.
- Windows Security > Virus & threat protection > Protection history: pick
  the M-Flasher entry and choose Actions > Restore (or Allow on device).
- Or wait a day or two, update Defender (Windows Update) and download
  again.

Please tell us the name Defender gave the threat (for example
"Trojan:Win32/Wacatac.B!ml") in an issue, so it can be reported.

More: https://github.com/jvitkauskas/m-flasher
