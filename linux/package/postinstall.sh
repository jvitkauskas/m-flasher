#!/bin/sh
set -e
# The SCSI generic driver now, and the udev rule for a unit already connected
# (both also take effect at the next boot or connection by themselves).
modprobe sg 2>/dev/null || true
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --action=add --subsystem-match=scsi_generic 2>/dev/null || true
    udevadm trigger --action=add --subsystem-match=usb --attr-match=idVendor=4c4a 2>/dev/null || true
fi
exit 0
