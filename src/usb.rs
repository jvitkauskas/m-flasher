//! The download mode over plain USB (libusb), for systems that do not pass
//! SCSI commands through to a USB disk (macOS): the app takes the interface
//! from the system's mass-storage driver and speaks USB mass-storage
//! Bulk-Only Transport itself. macOS lets only root take a device from its
//! driver (the app runs this in a root helper, see macos.rs), and asks the
//! user to allow it, which it cannot do over SSH.

use super::{Data, ScsiError, cmd_name};
use rusb::{Direction, GlobalContext, TransferType};
use std::time::Duration;

pub const VID: u16 = 0x4C4A;
pub const PID: u16 = 0x8057;
const TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"

pub struct Device {
    handle: rusb::DeviceHandle<GlobalContext>,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
    /// Bytes per packet of the data-in endpoint.
    in_packet: usize,
    tag: u32,
    /// Linux: the disk driver was detached from the interface.
    detached: bool,
}

fn failed(what: &str, e: rusb::Error) -> ScsiError {
    ScsiError::Failed(format!("{what}: {e}"))
}

fn needs_root(e: rusb::Error) -> ScsiError {
    ScsiError::NeedsAdmin(format!(
        "no permission to take the unit from the system's USB disk driver ({e}): {}",
        if cfg!(target_os = "macos") { "run M-Flasher with sudo" } else { "install the udev rule and reconnect the unit, or run as root" }
    ))
}

fn path_of(d: &rusb::Device<GlobalContext>) -> String {
    format!("usb:{}-{}", d.bus_number(), d.address())
}

fn is_root() -> bool {
    // SAFETY: a plain libc call without arguments
    unsafe { libc::geteuid() == 0 }
}

fn is_unit(d: &rusb::Device<GlobalContext>) -> bool {
    d.device_descriptor().is_ok_and(|dd| dd.vendor_id() == VID && dd.product_id() == PID)
}

/// The USB device of the unit at path, and its mass-storage interface with
/// its two bulk endpoints (and the data-in endpoint's packet size). Only a
/// device with the unit's USB IDs: taking any other disk from the system's
/// driver, a mounted one too, is never right (and on macOS not undone until
/// it is unplugged).
fn find(path: &str) -> Result<(rusb::Device<GlobalContext>, u8, u8, u8, usize), ScsiError> {
    let devices = rusb::devices().map_err(|e| failed("cannot list the USB devices", e))?;
    let dev = devices
        .iter()
        .find(|d| path_of(d) == path)
        .ok_or_else(|| ScsiError::Failed(format!("{path}: the unit is no longer connected")))?;
    if !is_unit(&dev) {
        return Err(ScsiError::Failed(format!("{path} is not a unit in download mode (USB {VID:04X}:{PID:04X})")));
    }
    let config = dev.active_config_descriptor().map_err(|e| failed("cannot read the USB configuration", e))?;
    for iface in config.interfaces() {
        for alt in iface.descriptors().filter(|a| a.class_code() == 8) {
            let bulk = |dir| {
                alt.endpoint_descriptors()
                    .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == dir)
                    .map(|e| (e.address(), e.max_packet_size() as usize))
            };
            if let (Some((i, packet)), Some((o, _))) = (bulk(Direction::In), bulk(Direction::Out)) {
                return Ok((dev, alt.interface_number(), i, o, packet));
            }
        }
    }
    Err(ScsiError::Failed(format!("{path}: no USB mass-storage interface")))
}

impl Device {
    pub fn open(path: &str) -> Result<Device, ScsiError> {
        let (dev, iface, ep_in, ep_out, in_packet) = find(path)?;
        let handle = dev.open().map_err(|e| match e {
            rusb::Error::Access => needs_root(e),
            e => failed("cannot open the unit", e),
        })?;
        // Linux: the disk driver lets go of the interface, and gets it back on drop
        let mut detached = false;
        if !cfg!(target_os = "macos") && handle.kernel_driver_active(iface).unwrap_or(false) {
            handle.detach_kernel_driver(iface).map_err(|e| match e {
                rusb::Error::Access => needs_root(e),
                e => failed("cannot take the unit from the system's USB disk driver", e),
            })?;
            detached = true;
        }
        let mut claimed = handle.claim_interface(iface);
        // macOS: the disk driver holds it (libusb cannot tell: it says no
        // driver is attached). Root can take the whole device from it.
        if claimed.is_err() && cfg!(target_os = "macos") {
            if !is_root() {
                return Err(ScsiError::NeedsAdmin("macOS's disk driver holds the unit: run M-Flasher with sudo".into()));
            }
            handle.detach_kernel_driver(iface).map_err(|e| failed("cannot take the unit from macOS's disk driver", e))?;
            // after a capture macOS takes a moment to offer the interface again
            // (up to 3 s: a busy or slow Mac takes longer than the usual few tenths)
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(100));
                claimed = handle.claim_interface(iface);
                if claimed.is_ok() {
                    break;
                }
            }
        }
        claimed.map_err(|e| match e {
            rusb::Error::Access | rusb::Error::Busy => needs_root(e),
            e if cfg!(target_os = "macos") => ScsiError::Failed(format!(
                "macOS did not let M-Flasher use the unit ({e}). It asks once to allow access to files on \
                 a removable volume: if that was refused, allow it in System Settings > Privacy & Security \
                 > Files & Folders. (Over SSH macOS cannot ask.)"
            )),
            e => failed("cannot take the unit's USB interface", e),
        })?;
        // Tags start somewhere random: the system's disk driver numbers its
        // commands too, and a status block it left behind must never pass for
        // the answer to one of ours (starting at 1 each time, it could).
        let tag = {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new().build_hasher().finish() as u32
        };
        let mut d = Device { handle, iface, ep_in, ep_out, in_packet, tag, detached };
        d.drain();
        Ok(d)
    }

    /// Reads away what the unit still has to send from before M-Flasher took
    /// it over (the status of the disk driver's last command, say), so that
    /// the first command starts in step. Normally there is nothing, and the
    /// first read just times out.
    fn drain(&mut self) {
        let mut buf = vec![0u8; self.in_packet.max(64) * 8];
        for _ in 0..4 {
            match self.handle.read_bulk(self.ep_in, &mut buf, DRAIN_TIMEOUT) {
                Ok(_) | Err(rusb::Error::Overflow) => {}
                Err(rusb::Error::Pipe) => {
                    let _ = self.handle.clear_halt(self.ep_in);
                }
                Err(_) => break,
            }
        }
    }

    /// One read of the data phase into `buf`. Read whole packets: asking
    /// libusb for less than the device then sends is an overflow, in which
    /// data is lost and the transport left out of step.
    fn read_data(&self, buf: &mut [u8]) -> Result<usize, rusb::Error> {
        let packet = self.in_packet.max(1);
        if buf.len() % packet == 0 {
            return self.handle.read_bulk(self.ep_in, buf, TIMEOUT);
        }
        let mut whole = vec![0u8; buf.len().next_multiple_of(packet)];
        let n = self.handle.read_bulk(self.ep_in, &mut whole, TIMEOUT)?;
        if n > buf.len() {
            // more than asked for: not this command's data
            return Err(rusb::Error::Overflow);
        }
        buf[..n].copy_from_slice(&whole[..n]);
        Ok(n)
    }

    /// Bulk-Only Mass Storage Reset, then clear both endpoints' halts.
    fn reset_recovery(&mut self) {
        let _ = self.handle.write_control(0x21, 0xFF, 0, self.iface as u16, &[], TIMEOUT);
        let _ = self.handle.clear_halt(self.ep_in);
        let _ = self.handle.clear_halt(self.ep_out);
    }

    /// The status block of the current command. A device out of step (on
    /// macOS, after the system's disk driver let go of it mid-command) sends
    /// something else first, data or an earlier command's status: skipped, a
    /// few times, rather than failing (an exact 13-byte read would overflow).
    fn read_csw(&mut self) -> Result<[u8; 13], rusb::Error> {
        let mut buf = [0u8; 512];
        for _ in 0..3 {
            match self.handle.read_bulk(self.ep_in, &mut buf, TIMEOUT) {
                Ok(13) if csw_tag(&buf) == Some(self.tag) => return Ok(buf[..13].try_into().unwrap()),
                Ok(_) => {}
                // a stalled endpoint: clear it and read again
                Err(rusb::Error::Pipe) => {
                    let _ = self.handle.clear_halt(self.ep_in);
                }
                Err(e) => return Err(e),
            }
        }
        Err(rusb::Error::Io)
    }

    fn transfer(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(ScsiError::Refused(format!("a command of {} bytes: 1 to 16 fit", cdb.len())));
        }
        self.tag = self.tag.wrapping_add(1);
        let (len, flags) = match &data {
            Data::None => (0, 0),
            Data::In(b) => (b.len(), 0x80),
            Data::Out(b) => (b.len(), 0),
        };
        let mut cbw = [0u8; 31];
        cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(len as u32).to_le_bytes());
        cbw[12] = flags;
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        let name = cmd_name(cdb);
        self.handle.write_bulk(self.ep_out, &cbw, TIMEOUT).map_err(|e| failed(&format!("sending command {name}"), e))?;
        // libusb ends a read at a short packet: the device may send less.
        // No data, no data phase (an empty write would send a zero-length
        // packet, which the device takes for the next command).
        let mut early = None;
        let phase = match data {
            Data::None => Ok(0),
            Data::In(b) if b.is_empty() => Ok(0),
            Data::Out(b) if b.is_empty() => Ok(0),
            Data::In(buf) => {
                // 13 bytes in place of more asked for may be a status block; 13
                // asked for are data, whatever they hold
                let status = |b: &[u8]| if b.len() > 13 { csw_tag(b) } else { None };
                let mut skipped = 0;
                loop {
                    match self.read_data(buf) {
                        // this command's status: the device sent no data
                        Ok(13) if status(buf) == Some(self.tag) => {
                            early = Some(buf[..13].try_into().unwrap());
                            break Ok(0);
                        }
                        // an earlier command's status, left over: the data follows
                        Ok(13) if skipped < 2 && status(buf).is_some() => skipped += 1,
                        Ok(n) => break Ok(n),
                        Err(rusb::Error::Pipe) => break Err(self.ep_in),
                        Err(e) => return Err(failed(&format!("command {name}: reading its data"), e)),
                    }
                }
            }
            Data::Out(buf) => match self.handle.write_bulk(self.ep_out, buf, TIMEOUT) {
                Ok(n) if n == buf.len() => Ok(n),
                Ok(n) => return Err(ScsiError::Failed(format!("command {name}: {n} of {} bytes sent", buf.len()))),
                Err(rusb::Error::Pipe) => Err(self.ep_out),
                Err(e) => return Err(failed(&format!("command {name}: sending its data"), e)),
            },
        };
        let moved = phase.unwrap_or_else(|ep| {
            // the device stalled the data phase: its status follows
            let _ = self.handle.clear_halt(ep);
            0
        });
        let csw = match early {
            Some(csw) => csw,
            None => self.read_csw().map_err(|e| failed(&format!("command {name}: reading its status"), e))?,
        };
        if u32::from_le_bytes(csw[0..4].try_into().unwrap()) != CSW_SIGNATURE
            || u32::from_le_bytes(csw[4..8].try_into().unwrap()) != self.tag
        {
            return Err(ScsiError::Failed(format!("command {name}: an invalid status block")));
        }
        match csw[12] {
            0 => Ok(moved),
            1 => Err(ScsiError::Refused(format!("command {name} failed (status 1)"))),
            s => Err(ScsiError::Failed(format!("command {name}: phase error (status {s})"))),
        }
    }
}

/// The tag of a status block (CSW), if `b` starts with one.
fn csw_tag(b: &[u8]) -> Option<u32> {
    (b.len() >= 13 && b[0..4] == CSW_SIGNATURE.to_le_bytes()).then(|| u32::from_le_bytes(b[4..8].try_into().unwrap()))
}

impl super::Scsi for Device {
    fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
        let r = self.transfer(cdb, data);
        // a refused command (status 1) leaves the transport in order; a failed
        // transfer is reset, for the next command (or its retry) to start clean
        if matches!(&r, Err(ScsiError::Failed(_))) {
            self.reset_recovery();
        }
        r
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.iface);
        // Linux: the disk driver gets the interface back. (macOS would
        // re-enumerate the device for that, which makes the chip leave download
        // mode: there it stays without a driver until it is unplugged.)
        if self.detached {
            let _ = self.handle.attach_kernel_driver(self.iface);
        }
    }
}

/// (path, description) of the download-mode units on USB.
pub fn find_devices() -> Vec<(String, String)> {
    let Ok(devices) = rusb::devices() else { return vec![] };
    devices
        .iter()
        .filter(is_unit)
        .map(|d| {
            let p = path_of(&d);
            let desc = format!("WL82 UBOOT1.00 (USB bus {}, device {})", d.bus_number(), d.address());
            (p, desc)
        })
        .collect()
}
