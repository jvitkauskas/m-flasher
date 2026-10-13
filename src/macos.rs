//! macOS: the unit through a root helper.
//!
//! Only root may take the unit from macOS's disk driver, and after that only
//! root may open it. The window stays a normal app (in the Dock, its files the
//! user's); when it first opens a unit it starts `M-Flasher --usb-helper` as
//! root, after the system's password dialog, and passes it each command over
//! a pipe. The helper is started with AuthorizationExecuteWithPrivileges, as a
//! child of the app: macOS lets a root process use the unit only when it
//! descends from a process of the user's (M-Flasher here, Terminal under sudo),
//! and then asks the user, once, to let M-Flasher access the removable volume
//! (the unit). Started any other way (osascript's administrator privileges, a
//! launchd job, "responsible for itself") it is refused without asking. The
//! call is deprecated, but nothing newer starts a root process that way.
//!
//! Run as root (sudo, the command line), the app uses the unit directly.

use super::usb;
use super::{Data, Scsi, ScsiError};
use std::ffi::{CString, c_char, c_void};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;

/// `M-Flasher --usb-helper`: the root helper.
pub const USB_HELPER: &str = "--usb-helper";

const FIND: u8 = 1;
const OPEN: u8 = 2;
const EXECUTE: u8 = 3;
const CLOSE: u8 = 4;

const OK: u8 = 0;
const FAILED: u8 = 1;
const NEEDS_ADMIN: u8 = 2;

fn is_root() -> bool {
    // SAFETY: a plain libc call without arguments
    unsafe { libc::geteuid() == 0 }
}

// ------------------------------------------------------------ the app side ---

/// The pipe to the running helper.
struct Helper {
    pipe: File,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

impl Helper {
    /// Starts the helper after the system's password dialog.
    fn start() -> Result<Helper, ScsiError> {
        let refused = || ScsiError::NeedsAdmin("M-Flasher needs an administrator's password to reach the unit".into());
        let exe = std::env::current_exe().map_err(|e| ScsiError::Failed(e.to_string()))?;
        let path = CString::new(exe.as_os_str().as_bytes()).map_err(|e| ScsiError::Failed(e.to_string()))?;
        let arg = CString::new(USB_HELPER).unwrap();
        let argv = [arg.as_ptr(), std::ptr::null()];
        let prompt = c"M-Flasher needs administrator rights to reach the unit's USB download mode.";
        // SAFETY: Security framework calls with initialised items; every
        // pointer outlives the call it is passed to.
        unsafe {
            let mut auth = std::ptr::null_mut();
            if AuthorizationCreate(std::ptr::null(), std::ptr::null(), 0, &mut auth) != 0 {
                return Err(refused());
            }
            let mut right = AuthorizationItem {
                name: c"system.privilege.admin".as_ptr(),
                value_length: 0,
                value: std::ptr::null_mut(),
                flags: 0,
            };
            let rights = AuthorizationItemSet { count: 1, items: &mut right };
            let mut prompt_item = AuthorizationItem {
                name: c"prompt".as_ptr(),
                value_length: prompt.to_bytes().len(),
                value: prompt.as_ptr() as *mut c_void,
                flags: 0,
            };
            let environment = AuthorizationItemSet { count: 1, items: &mut prompt_item };
            // interaction allowed, extend rights, preauthorize
            let mut pipe: *mut libc::FILE = std::ptr::null_mut();
            let ok = AuthorizationCopyRights(auth, &rights, &environment, 1 | 2 | 16, std::ptr::null_mut()) == 0
                && AuthorizationExecuteWithPrivileges(auth, path.as_ptr(), 0, argv.as_ptr(), &mut pipe) == 0
                && !pipe.is_null();
            AuthorizationFree(auth, 0);
            if !ok {
                return Err(refused());
            }
            // the helper's standard input and output, both ways
            let fd = libc::dup(libc::fileno(pipe));
            libc::fclose(pipe);
            Ok(Helper { pipe: File::from_raw_fd(fd) })
        }
    }

    fn call(&mut self, request: &[u8]) -> std::io::Result<(u8, Vec<u8>)> {
        self.pipe.write_all(&(request.len() as u32).to_le_bytes())?;
        self.pipe.write_all(request)?;
        let mut head = [0u8; 5];
        self.pipe.read_exact(&mut head)?;
        let mut payload = vec![0u8; u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize];
        self.pipe.read_exact(&mut payload)?;
        Ok((head[0], payload))
    }
}

/// Sends a request to the helper (starting it first if `start`).
fn ask(request: &[u8], start: bool) -> Result<Vec<u8>, ScsiError> {
    let mut helper = HELPER.lock().unwrap();
    if helper.is_none() {
        if !start {
            return Err(ScsiError::Failed("no helper".into()));
        }
        *helper = Some(Helper::start()?);
    }
    match helper.as_mut().unwrap().call(request) {
        Ok((OK, payload)) => Ok(payload),
        Ok((NEEDS_ADMIN, m)) => Err(ScsiError::NeedsAdmin(String::from_utf8_lossy(&m).into())),
        Ok((_, m)) => Err(ScsiError::Failed(String::from_utf8_lossy(&m).into())),
        Err(e) => {
            *helper = None;
            Err(ScsiError::Failed(format!("the USB helper stopped: {e}")))
        }
    }
}

pub enum Device {
    /// Run as root: the unit itself.
    Direct(usb::Device),
    /// Through the helper.
    Helper,
}

impl Device {
    pub fn open(path: &str) -> Result<Device, ScsiError> {
        if is_root() {
            return usb::Device::open(path).map(Device::Direct);
        }
        let mut request = vec![OPEN];
        request.extend(path.as_bytes());
        ask(&request, true).map(|_| Device::Helper)
    }
}

impl Scsi for Device {
    fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
        if let Device::Direct(d) = self {
            return d.execute(cdb, data);
        }
        let mut request = vec![EXECUTE, cdb.len() as u8];
        request.extend(cdb);
        match &data {
            Data::None => request.extend([0, 0, 0, 0, 0]),
            Data::In(b) => {
                request.push(1);
                request.extend((b.len() as u32).to_le_bytes());
            }
            Data::Out(b) => {
                request.push(2);
                request.extend((b.len() as u32).to_le_bytes());
                request.extend(*b);
            }
        }
        let payload = ask(&request, false)?;
        Ok(match data {
            Data::None => 0,
            Data::In(b) => {
                // the helper sends only the bytes received
                let n = payload.len().min(b.len());
                b[..n].copy_from_slice(&payload[..n]);
                n
            }
            Data::Out(b) => b.len(),
        })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Device::Helper = self {
            let _ = ask(&[CLOSE], false);
        }
    }
}

/// (path, description) of the download-mode units. A unit taken from the
/// disk driver is root's only until it is unplugged, and libusb cannot list it
/// for a normal process: the helper lists them once it runs, before that the
/// I/O registry does.
pub fn find_devices() -> Vec<(String, String)> {
    if is_root() {
        return usb::find_devices();
    }
    if let Ok(list) = ask(&[FIND], false) {
        return String::from_utf8_lossy(&list)
            .lines()
            .filter_map(|l| l.split_once('\t').map(|(p, d)| (p.to_string(), d.to_string())))
            .collect();
    }
    registry_units()
}

/// The units in the I/O registry, by vendor and product ID, named as libusb
/// names them (bus: locationID >> 24, then the USB address).
fn registry_units() -> Vec<(String, String)> {
    let mut out = vec![];
    // SAFETY: IOKit and CoreFoundation calls; every object obtained is released.
    unsafe {
        let number = |service: u32, key: &std::ffi::CStr| -> Option<i64> {
            let k = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), CF_STRING_UTF8);
            let v = IORegistryEntryCreateCFProperty(service, k, std::ptr::null(), 0);
            CFRelease(k);
            if v.is_null() {
                return None;
            }
            let mut n = 0i64;
            let ok = CFNumberGetValue(v, CF_NUMBER_SINT64, &mut n as *mut i64 as *mut c_void);
            CFRelease(v);
            ok.then_some(n)
        };
        let mut services = 0u32;
        if IOServiceGetMatchingServices(0, IOServiceMatching(c"IOUSBHostDevice".as_ptr()), &mut services) != 0 {
            return out;
        }
        loop {
            let service = IOIteratorNext(services);
            if service == 0 {
                break;
            }
            if number(service, c"idVendor") == Some(usb::VID as i64) && number(service, c"idProduct") == Some(usb::PID as i64) {
                if let (Some(location), Some(address)) = (number(service, c"locationID"), number(service, c"USB Address")) {
                    let bus = (location >> 24) & 0xff;
                    out.push((format!("usb:{bus}-{address}"), format!("WL82 UBOOT1.00 (USB bus {bus}, device {address})")));
                }
            }
            IOObjectRelease(service);
        }
        IOObjectRelease(services);
    }
    out
}

// ------------------------------------------------------- the root helper ---

/// The helper: requests on standard input, answers on standard output, until
/// the app closes the pipe. The exit code.
pub fn usb_helper() -> i32 {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut device: Option<usb::Device> = None;
    loop {
        let mut len = [0u8; 4];
        if input.read_exact(&mut len).is_err() {
            return 0; // the app is gone
        }
        let mut request = vec![0u8; u32::from_le_bytes(len) as usize];
        if input.read_exact(&mut request).is_err() || request.is_empty() {
            return 0;
        }
        let answer: Result<Vec<u8>, ScsiError> = match request[0] {
            FIND => Ok(usb::find_devices().iter().map(|(p, d)| format!("{p}\t{d}\n")).collect::<String>().into_bytes()),
            OPEN => {
                device = None;
                usb::Device::open(&String::from_utf8_lossy(&request[1..])).map(|d| {
                    device = Some(d);
                    vec![]
                })
            }
            EXECUTE => match device.as_mut() {
                None => Err(ScsiError::Failed("no unit open".into())),
                Some(d) => execute(d, &request[1..]),
            },
            CLOSE => {
                device = None;
                Ok(vec![])
            }
            _ => Err(ScsiError::Failed("unknown request".into())),
        };
        let (status, payload) = match answer {
            Ok(p) => (OK, p),
            Err(ScsiError::NeedsAdmin(m)) => (NEEDS_ADMIN, m.into_bytes()),
            Err(ScsiError::Failed(m)) => (FAILED, m.into_bytes()),
        };
        let mut frame = vec![status];
        frame.extend((payload.len() as u32).to_le_bytes());
        frame.extend(payload);
        if output.write_all(&frame).and_then(|_| output.flush()).is_err() {
            return 0;
        }
    }
}

/// EXECUTE: cdb length, cdb, direction (0 none, 1 in, 2 out), data length,
/// the data to send.
fn execute(d: &mut usb::Device, r: &[u8]) -> Result<Vec<u8>, ScsiError> {
    let bad = || ScsiError::Failed("bad request".into());
    let n = *r.first().ok_or_else(bad)? as usize;
    let cdb = r.get(1..1 + n).ok_or_else(bad)?;
    let dir = *r.get(1 + n).ok_or_else(bad)?;
    let len = u32::from_le_bytes(r.get(2 + n..6 + n).ok_or_else(bad)?.try_into().unwrap()) as usize;
    match dir {
        0 => d.execute(cdb, Data::None).map(|_| vec![]),
        1 => {
            let mut buf = vec![0u8; len];
            let got = d.execute(cdb, Data::In(&mut buf))?;
            buf.truncate(got);
            Ok(buf)
        }
        _ => d.execute(cdb, Data::Out(r.get(6 + n..6 + n + len).ok_or_else(bad)?)).map(|_| vec![]),
    }
}

// ------------------------------------- IOKit, CoreFoundation, Security ---

const CF_STRING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: isize = 4;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, existing: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: *const c_void, allocator: *const c_void, options: u32)
        -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, the_type: isize, value: *mut c_void) -> bool;
    fn CFRelease(cf: *const c_void);
}

#[repr(C)]
struct AuthorizationItem {
    name: *const c_char,
    value_length: usize,
    value: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct AuthorizationItemSet {
    count: u32,
    items: *mut AuthorizationItem,
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn AuthorizationCreate(rights: *const AuthorizationItemSet, environment: *const AuthorizationItemSet,
                           flags: u32, authorization: *mut *mut c_void) -> i32;
    fn AuthorizationCopyRights(authorization: *mut c_void, rights: *const AuthorizationItemSet,
                               environment: *const AuthorizationItemSet, flags: u32,
                               authorized: *mut *mut AuthorizationItemSet) -> i32;
    fn AuthorizationExecuteWithPrivileges(authorization: *mut c_void, path: *const c_char, options: u32,
                                          arguments: *const *const c_char, pipe: *mut *mut libc::FILE) -> i32;
    fn AuthorizationFree(authorization: *mut c_void, flags: u32) -> i32;
}
