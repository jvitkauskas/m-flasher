M-Flasher
=========

Back up, restore and reinstall the firmware of an M-VAVE FM-1 or SMK-37
Pro / Elite, and install m-bootloader.

1. Let M-Flasher reach the unit as you: install the udev rule once,
     sudo cp 70-m-flasher.rules /etc/udev/rules.d/
     sudo udevadm control --reload
   and connect the unit (again, if it was connected). It uses the SCSI
   generic driver: if there is no /dev/sg*, run "sudo modprobe sg".
2. Run ./m-flasher, as you: under sudo the window cannot reach the desktop.
3. Put the unit in download mode and connect it with a USB data cable:
   - with m-bootloader: hold REC (SMK-37: SEQ REC) while switching on;
   - Felucca, SLOOP, Snoop37: hold OCT- and OCT+ for 5 s while it runs.
4. Start with "Back up the whole flash". Backups go to
   ~/Documents/M-Flasher/backups.

With arguments it is a command-line tool: ./m-flasher info, backup,
restore, firmware, bootloader (./m-flasher --help).

m-flasher.png is the icon, for a launcher of your own.

More: https://github.com/jvitkauskas/m-flasher
