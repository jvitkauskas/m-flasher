//! SCSI pass-through to the chip's USB download mode, which shows up as a USB
//! disk (vendor "WL82", product "UBOOT1.00", USB 4C4A:8057).
//!
//! Linux: the SCSI generic driver (/dev/sgN, SG_IO). Windows:
//! \\.\PhysicalDriveN with IOCTL_SCSI_PASS_THROUGH_DIRECT, which needs
//! administrator rights. macOS (and Linux built with the `usb` feature): USB
//! Bulk-Only Transport through libusb, see usb.rs. Ported from jl-uboot-tool's scsiio (MIT, (c) 2023
//! Andrey Grigoryev).

use std::fmt;

pub const VENDOR: &str = "WL82";
pub const PRODUCT: &str = "UBOOT1.00";
#[cfg(any(windows, all(target_os = "linux", not(feature = "usb"))))]
const TIMEOUT_S: u32 = 10;

#[derive(Debug, Clone)]
pub enum ScsiError {
    /// No permission to open the device (Linux: udev rule or root; Windows: administrator).
    NeedsAdmin(String),
    Failed(String),
}

impl fmt::Display for ScsiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ScsiError::NeedsAdmin(s) | ScsiError::Failed(s) => f.write_str(s),
        }
    }
}

pub enum Data<'a> {
    #[allow(dead_code)]
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

pub trait Scsi: Send {
    /// Runs one command; returns the number of data bytes transferred, which
    /// can be fewer than asked for when the device sends less.
    fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError>;

    fn inquiry(&mut self) -> Result<(String, String), ScsiError> {
        let mut buf = [0u8; 36];
        self.execute(&[0x12, 0, 0, 0, 36, 0], Data::In(&mut buf))?;
        let s = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
        Ok((s(&buf[8..16]), s(&buf[16..32])))
    }
}

fn cmd_name(cdb: &[u8]) -> String {
    format!("0x{:02x}{:02x}", cdb[0], cdb.get(1).copied().unwrap_or(0))
}

// ---------------------------------------------------------------- Linux ---

#[cfg(all(target_os = "linux", not(feature = "usb")))]
mod imp {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct SgIoHdr {
        interface_id: i32,
        dxfer_direction: i32,
        cmd_len: u8,
        mx_sb_len: u8,
        iovec_count: u16,
        dxfer_len: u32,
        dxferp: *mut libc::c_void,
        cmdp: *const u8,
        sbp: *mut u8,
        timeout: u32,
        flags: u32,
        pack_id: i32,
        usr_ptr: *mut libc::c_void,
        status: u8,
        masked_status: u8,
        msg_status: u8,
        sb_len_wr: u8,
        host_status: u16,
        driver_status: u16,
        resid: i32,
        duration: u32,
        info: u32,
    }

    const SG_IO: libc::c_ulong = 0x2285;

    pub struct Device {
        file: File,
    }

    impl Device {
        pub fn open(path: &str) -> Result<Device, ScsiError> {
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => Ok(Device { file }),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(ScsiError::NeedsAdmin(format!(
                    "no permission to open {path}: install the udev rule and reconnect the unit (see README.txt), or run as root"
                ))),
                Err(e) => Err(ScsiError::Failed(format!("cannot open {path}: {e}"))),
            }
        }
    }

    impl Scsi for Device {
        fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
            let mut sense = [0u8; 32];
            let (dir, ptr, len) = match data {
                Data::None => (-1, std::ptr::null_mut(), 0),
                Data::Out(b) => (-2, b.as_ptr() as *mut libc::c_void, b.len()),
                Data::In(b) => (-3, b.as_mut_ptr() as *mut libc::c_void, b.len()),
            };
            let mut h = SgIoHdr {
                interface_id: b'S' as i32,
                dxfer_direction: dir,
                cmd_len: cdb.len() as u8,
                mx_sb_len: sense.len() as u8,
                iovec_count: 0,
                dxfer_len: len as u32,
                dxferp: ptr,
                cmdp: cdb.as_ptr(),
                sbp: sense.as_mut_ptr(),
                timeout: TIMEOUT_S * 1000,
                flags: 0,
                pack_id: 0,
                usr_ptr: std::ptr::null_mut(),
                status: 0,
                masked_status: 0,
                msg_status: 0,
                sb_len_wr: 0,
                host_status: 0,
                driver_status: 0,
                resid: 0,
                duration: 0,
                info: 0,
            };
            // SAFETY: h points at valid buffers that outlive the call.
            let r = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut h as *mut SgIoHdr) };
            if r < 0 {
                return Err(ScsiError::Failed(format!("SG_IO failed: {}", std::io::Error::last_os_error())));
            }
            if h.info & 1 != 0 {
                return Err(ScsiError::Failed(format!(
                    "command {} failed: status 0x{:02x}, host 0x{:02x}, driver 0x{:02x}",
                    cmd_name(cdb), h.status, h.host_status, h.driver_status
                )));
            }
            Ok(len.saturating_sub(h.resid.max(0) as usize))
        }
    }

    /// (path, description) of the download-mode disks.
    pub fn find_devices() -> Vec<(String, String)> {
        let mut out = vec![];
        let Ok(dir) = std::fs::read_dir("/sys/class/scsi_generic") else { return out };
        let mut names: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for name in names {
            let base = format!("/sys/class/scsi_generic/{name}/device");
            let read = |f: &str| std::fs::read_to_string(format!("{base}/{f}")).map(|s| s.trim().to_string());
            if let (Ok(v), Ok(m)) = (read("vendor"), read("model")) {
                if v == VENDOR && m == PRODUCT {
                    out.push((format!("/dev/{name}"), format!("{v} {m} (/dev/{name})")));
                }
            }
        }
        out
    }
}

// -------------------------------------------------------------- Windows ---

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x4D014;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D1400;

    #[repr(C)]
    struct Sptd {
        length: u16,
        scsi_status: u8,
        path_id: u8,
        target_id: u8,
        lun: u8,
        cdb_length: u8,
        sense_info_length: u8,
        data_in: u8,
        data_transfer_length: u32,
        timeout_value: u32,
        data_buffer: *mut c_void,
        sense_info_offset: u32,
        cdb: [u8; 16],
    }

    #[repr(C)]
    struct SptdWithSense {
        sptd: Sptd,
        sense: [u8; 32],
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open_raw(path: &str, access: u32) -> Result<HANDLE, ScsiError> {
        let w = wide(path);
        // SAFETY: w is a valid NUL-terminated string.
        let h = unsafe {
            CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: plain FFI call.
            let e = unsafe { GetLastError() };
            if e == ERROR_ACCESS_DENIED {
                return Err(ScsiError::NeedsAdmin(format!("access to {path} denied: run M-Flasher as administrator")));
            }
            return Err(ScsiError::Failed(format!("cannot open {path}: error {e}")));
        }
        Ok(h)
    }

    pub struct Device {
        h: HANDLE,
    }

    // SAFETY: the handle is only used by one thread at a time (behind &mut self).
    unsafe impl Send for Device {}

    impl Device {
        pub fn open(path: &str) -> Result<Device, ScsiError> {
            Ok(Device { h: open_raw(path, GENERIC_READ | GENERIC_WRITE)? })
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: h came from CreateFileW.
            unsafe { CloseHandle(self.h) };
        }
    }

    impl Scsi for Device {
        fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
            let mut s: SptdWithSense = unsafe { std::mem::zeroed() };
            s.sptd.length = std::mem::size_of::<Sptd>() as u16;
            s.sptd.target_id = 1;
            s.sptd.cdb_length = cdb.len() as u8;
            s.sptd.sense_info_length = 32;
            s.sptd.sense_info_offset = std::mem::offset_of!(SptdWithSense, sense) as u32;
            s.sptd.timeout_value = TIMEOUT_S;
            s.sptd.cdb[..cdb.len()].copy_from_slice(cdb);
            match data {
                Data::None => s.sptd.data_in = 2,
                Data::Out(b) => {
                    s.sptd.data_in = 0;
                    s.sptd.data_transfer_length = b.len() as u32;
                    s.sptd.data_buffer = b.as_ptr() as *mut c_void;
                }
                Data::In(b) => {
                    s.sptd.data_in = 1;
                    s.sptd.data_transfer_length = b.len() as u32;
                    s.sptd.data_buffer = b.as_mut_ptr() as *mut c_void;
                }
            }
            let mut got = 0u32;
            let size = std::mem::size_of::<SptdWithSense>() as u32;
            let p = &mut s as *mut SptdWithSense as *mut c_void;
            // SAFETY: s and the data buffer outlive the call.
            let ok = unsafe { DeviceIoControl(self.h, IOCTL_SCSI_PASS_THROUGH_DIRECT, p, size, p, size, &mut got, std::ptr::null_mut()) };
            if ok == 0 {
                // SAFETY: plain FFI call.
                let e = unsafe { GetLastError() };
                return Err(ScsiError::Failed(format!("command {} failed: error {e}", cmd_name(cdb))));
            }
            if s.sptd.scsi_status != 0 {
                return Err(ScsiError::Failed(format!("command {} failed: status 0x{:02x}", cmd_name(cdb), s.sptd.scsi_status)));
            }
            // the driver sets it to the number of bytes transferred
            Ok(s.sptd.data_transfer_length as usize)
        }
    }

    /// (vendor, product) from the storage driver: no SCSI command, no
    /// administrator rights.
    fn query_ids(path: &str) -> Option<(String, String)> {
        let h = open_raw(path, 0).ok()?;
        let query = [0u8; 12]; // StorageDeviceProperty, PropertyStandardQuery
        let mut out = vec![0u8; 1024];
        let mut got = 0u32;
        // SAFETY: buffers are valid for the call.
        let ok = unsafe {
            DeviceIoControl(h, IOCTL_STORAGE_QUERY_PROPERTY, query.as_ptr() as *const c_void, 12,
                out.as_mut_ptr() as *mut c_void, 1024, &mut got, std::ptr::null_mut())
        };
        // SAFETY: h came from CreateFileW.
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        let raw = &out[..got as usize];
        let field = |at: usize| -> String {
            let off = u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as usize;
            if off == 0 || off >= raw.len() {
                return String::new();
            }
            let end = raw[off..].iter().position(|&c| c == 0).map(|p| off + p).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[off..end]).trim().to_string()
        };
        (raw.len() >= 20).then(|| (field(12), field(16)))
    }

    pub fn find_devices() -> Vec<(String, String)> {
        let mut out = vec![];
        for n in 0..64 {
            let path = format!(r"\\.\PhysicalDrive{n}");
            if let Some((v, p)) = query_ids(&path) {
                if v == VENDOR && p == PRODUCT {
                    out.push((path, format!("{v} {p} (disk {n})")));
                }
            }
        }
        out
    }
}

// --------------------------------------------------- macOS: USB directly ---

#[cfg(any(target_os = "macos", all(target_os = "linux", feature = "usb")))]
#[path = "usb.rs"]
mod usb;
#[cfg(all(target_os = "linux", feature = "usb"))]
use usb as imp;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
pub mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

// ---------------------------------------------------------------- other ---

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use super::*;

    pub struct Device;

    impl Device {
        pub fn open(_path: &str) -> Result<Device, ScsiError> {
            Err(ScsiError::Failed("this system is not supported: use Windows, Linux or macOS".into()))
        }
    }

    impl Scsi for Device {
        fn execute(&mut self, _cdb: &[u8], _data: Data) -> Result<usize, ScsiError> {
            unreachable!()
        }
    }

    pub fn find_devices() -> Vec<(String, String)> {
        vec![]
    }
}

pub use imp::{Device, find_devices};
