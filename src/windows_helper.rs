//! Windows: the unit through an administrator helper.
//!
//! Finding the unit needs no rights, but sending it SCSI commands needs an
//! administrator. So the app runs as the user (its window, its files the
//! user's), and when it first opens a unit without the rights it starts
//! `M-Flasher --scsi-helper PIPE PID` as administrator, after Windows asks
//! (UAC), and passes it each command over a named pipe: the requests and
//! answers of the macOS helper (macos.rs). The helper runs until the app
//! closes the pipe, so a unit reconnected later doesn't ask again.
//!
//! Each side checks the other: the app creates the pipe and talks only to the
//! process it started; the helper talks only to the process given on its
//! command line, and only if that is this same M-Flasher.exe, and opens only a
//! disk it finds on the unit's USB IDs itself.
//!
//! Run as administrator (an administrator prompt), the app uses the unit
//! directly.

use super::imp::{Disk, find_devices};
use super::{Data, Scsi, ScsiError};
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, SetNamedPipeHandleState,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};

/// `M-Flasher --scsi-helper PIPE PID`: the administrator helper.
pub const SCSI_HELPER: &str = "--scsi-helper";
const PIPE_PREFIX: &str = r"\\.\pipe\m-flasher-";

const OPEN: u8 = 2;
const EXECUTE: u8 = 3;
const CLOSE: u8 = 4;

const OK: u8 = 0;
const FAILED: u8 = 1;
const NEEDS_ADMIN: u8 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ------------------------------------------------------------ the app side ---

/// The pipe to the running helper.
struct Helper {
    pipe: File,
    process: HANDLE,
}

// SAFETY: the process handle is only waited on and closed, behind the mutex.
unsafe impl Send for Helper {}

impl Drop for Helper {
    fn drop(&mut self) {
        // SAFETY: process came from ShellExecuteExW.
        unsafe { CloseHandle(self.process) };
    }
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

impl Helper {
    /// Starts the helper after Windows asks for administrator rights.
    fn start() -> Result<Helper, ScsiError> {
        let failed = |what: String| ScsiError::Failed(format!("cannot start the administrator helper: {what}"));
        // SAFETY: plain FFI call.
        let app = unsafe { GetCurrentProcessId() };
        // unguessable, so no other program has it ready; FIRST_PIPE_INSTANCE refuses one that does
        let nonce = {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new().build_hasher().finish()
        };
        let name = format!("{PIPE_PREFIX}{app}-{nonce:016x}");
        // SAFETY: the name is a valid NUL-terminated string; default security.
        let h = unsafe {
            CreateNamedPipeW(wide(&name).as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS, 1, 1 << 16, 1 << 16, 0, std::ptr::null())
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: plain FFI call.
            return Err(failed(format!("no pipe: error {}", unsafe { GetLastError() })));
        }
        // SAFETY: h is a pipe handle of ours, closed with the file.
        let pipe = unsafe { File::from_raw_handle(h as _) };
        let exe = std::env::current_exe().map_err(|e| failed(e.to_string()))?;
        let exe = wide(&exe.to_string_lossy());
        let params = wide(&format!("{SCSI_HELPER} {name} {app}"));
        let verb = wide("runas");
        // SAFETY: a zeroed SHELLEXECUTEINFOW is valid, its strings outlive the
        // call; COM as ShellExecuteExW wants it.
        let (ok, error, process) = unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = exe.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = 0; // SW_HIDE: it has no window anyway
            let com = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
            let ok = ShellExecuteExW(&mut info);
            let error = GetLastError();
            if com >= 0 {
                CoUninitialize();
            }
            (ok != 0, error, info.hProcess)
        };
        if !ok {
            return Err(if error == ERROR_CANCELLED {
                ScsiError::NeedsAdmin("M-Flasher needs administrator rights to reach the unit: reconnect it to be asked again".into())
            } else {
                failed(format!("error {error}"))
            });
        }
        if process.is_null() {
            return Err(failed("no process".into()));
        }
        let helper = Helper { pipe, process };
        let raw = helper.pipe.as_raw_handle() as HANDLE;
        // SAFETY: process is valid until the helper is dropped.
        let helper_id = unsafe { GetProcessId(helper.process) };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            // non-blocking: fails with ERROR_PIPE_CONNECTED once the helper is there
            // SAFETY: raw is our pipe; no OVERLAPPED, as it is non-blocking.
            if unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) } == 0 && unsafe { GetLastError() } == ERROR_PIPE_CONNECTED {
                break;
            }
            // SAFETY: process is valid until the helper is dropped.
            if unsafe { WaitForSingleObject(helper.process, 0) } != WAIT_TIMEOUT {
                return Err(failed("it stopped".into()));
            }
            if Instant::now() > deadline {
                return Err(failed("it did not answer".into()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut client = 0u32;
        // SAFETY: raw is our pipe; client is valid for the call.
        if unsafe { GetNamedPipeClientProcessId(raw, &mut client) } == 0 || client != helper_id {
            return Err(failed("another program connected to its pipe".into()));
        }
        let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
        // SAFETY: raw is our pipe; mode is valid for the call.
        if unsafe { SetNamedPipeHandleState(raw, &mode, std::ptr::null(), std::ptr::null()) } == 0 {
            // SAFETY: plain FFI call.
            return Err(failed(format!("pipe mode: error {}", unsafe { GetLastError() })));
        }
        Ok(helper)
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
            Err(ScsiError::Failed(format!("the administrator helper stopped: {e}")))
        }
    }
}

/// Opens the unit in the helper, starting it if it isn't running.
pub fn open(path: &str) -> Result<(), ScsiError> {
    let mut request = vec![OPEN];
    request.extend(path.as_bytes());
    ask(&request, true).map(|_| ())
}

/// A command to the unit open in the helper.
pub fn execute(cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
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

/// Closes the unit in the helper, which keeps running.
pub fn close() {
    let _ = ask(&[CLOSE], false);
}

// ------------------------------------------------- the administrator helper ---

/// The helper: requests over the app's pipe, answers back, until the app
/// closes it. The exit code.
pub fn scsi_helper(args: &[String]) -> i32 {
    let (Some(name), Some(app)) = (args.get(1), args.get(2).and_then(|p| p.parse::<u32>().ok())) else {
        return 2;
    };
    if !name.starts_with(PIPE_PREFIX) {
        return 2;
    }
    let Ok(mut pipe) = std::fs::OpenOptions::new().read(true).write(true).open(name) else {
        return 1;
    };
    let mut server = 0u32;
    // SAFETY: the pipe is open; server is valid for the call.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut server) } == 0 || server != app || !same_program(server) {
        return 1;
    }
    let mut device: Option<Disk> = None;
    loop {
        let mut len = [0u8; 4];
        if pipe.read_exact(&mut len).is_err() {
            return 0; // the app is gone
        }
        let mut request = vec![0u8; u32::from_le_bytes(len) as usize];
        if pipe.read_exact(&mut request).is_err() || request.is_empty() {
            return 0;
        }
        let answer: Result<Vec<u8>, ScsiError> = match request[0] {
            OPEN => {
                device = None;
                let path = String::from_utf8_lossy(&request[1..]).into_owned();
                // only the unit: never a disk the app names
                if !find_devices().iter().any(|(p, _)| *p == path) {
                    Err(ScsiError::Failed(format!("{path} is not a unit in download mode")))
                } else {
                    Disk::open(&path).map(|d| {
                        device = Some(d);
                        vec![]
                    })
                }
            }
            EXECUTE => match device.as_mut() {
                None => Err(ScsiError::Failed("no unit open".into())),
                Some(d) => execute_request(d, &request[1..]),
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
        if pipe.write_all(&frame).and_then(|_| pipe.flush()).is_err() {
            return 0;
        }
    }
}

/// Whether the process runs this same executable: another program could start
/// the helper for a pipe of its own.
fn same_program(pid: u32) -> bool {
    // SAFETY: plain FFI call; the handle is closed below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: buf holds len characters; h is valid.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    // SAFETY: h came from OpenProcess.
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return false;
    }
    let theirs = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    // canonical: the same file however each was started (short names, case)
    match (std::fs::canonicalize(theirs), std::env::current_exe().and_then(std::fs::canonicalize)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
}

/// EXECUTE: cdb length, cdb, direction (0 none, 1 in, 2 out), data length,
/// the data to send.
fn execute_request(d: &mut Disk, r: &[u8]) -> Result<Vec<u8>, ScsiError> {
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
