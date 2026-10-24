//! Command-line mode (the same operations as the window):
//!
//! ```text
//! m-flasher find | download-mode | info | backup [DIR] | restore BACKUP.bin
//!           firmware PACKAGE.fwsc [--with-bootloader] [--wipe-settings]
//!           bootloader [--stock PACKAGE.fwsc | --file UBOOT.boot]
//!           run-ram IMAGE --addr ADDR [--call OPCODE]... [--read ADDR:LEN -o OUT]
//! options:  --device PATH  --board fm1|smk37  --backup-dir DIR  --yes
//! ```

use crate::jl::Connection;
use crate::ops::{Error, Plan, Session, board_from_key};
use crate::scsi::{Device, find_devices};
use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "usage: m-flasher [find | download-mode | info | backup [DIR] | restore BACKUP.bin |
                 firmware PACKAGE.fwsc [--with-bootloader] [--wipe-settings] |
                 bootloader [--stock PACKAGE.fwsc | --file UBOOT.boot] |
                 run-ram IMAGE --addr ADDR [--call OPCODE]... [--read ADDR:LEN -o OUT]]
       options: --device PATH  --board fm1|smk37  --backup-dir DIR  --yes
       without arguments: the window (`m-flasher run-ram --help`: the run-ram command)";

const RUN_RAM_HELP: &str = "usage: m-flasher run-ram IMAGE --addr ADDR [--call OPCODE]... [--read ADDR:LEN -o OUT] [--device PATH]

Runs your own program in the unit's RAM, through the chip's download mode, and
talks to it. It does not start M-Flasher's flash loader and does not touch the
flash; it works the same when the loader is already running. The program runs
until the unit is switched off and on again.

  IMAGE         the program: a flat binary, linked for ADDR, whose first byte is
                its entry (the ROM calls it the way it calls the flash loader)
  --addr ADDR   hex (0x optional), within 0x01C20000..0x01C30000 (the flash
                loader's two 32 KiB slots, the only RAM known to be safe); the
                image must end before 0x01C30000. The uploaded program replaces
                a running M-Flasher loader that is in the same slot: refused
  --call OPCODE a 16-bit hex opcode (repeat for more): sends the 16-byte command
                (opcode big endian, rest FF) and prints the 16-byte answer in hex,
                in which the program echoes the opcode. Not sent: FB00 FB01 FB04
                FB06 FB08 (flash erase and write, memory write, jump)
  --read A:LEN  reads LEN bytes at address A (hex, 0x optional; LEN decimal or
                0x hex, up to 16 MiB) with FD61 commands of at most 4096 bytes:
                `FD 61`, A (u32), the chunk length (u16), rest FF. The program
                has to implement it. Needs -o OUT, the file to write
  --device PATH the unit, when more than one is connected

This runs arbitrary code on the chip. Use it only with programs you trust.";

pub fn default_backup_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    // under sudo the backups still go to the user's own Documents
    #[cfg(unix)]
    let home = sudo_user_home().unwrap_or(home);
    let docs = home.join("Documents");
    (if docs.is_dir() { docs } else { home }).join("M-Flasher").join("backups")
}

/// The home folder of the user who ran sudo, when running as root under it.
#[cfg(unix)]
fn sudo_user_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let user = std::ffi::CString::new(std::env::var("SUDO_USER").ok()?).ok()?;
    // SAFETY: getpwnam with a valid C string; the entry is read at once, before
    // any other call that could overwrite it
    unsafe {
        if libc::geteuid() != 0 {
            return None;
        }
        let pw = libc::getpwnam(user.as_ptr());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
    }
}

fn progress(done: usize, total: usize) {
    eprint!("\r  {:3}%", 100 * done / total.max(1));
    if done >= total {
        eprintln!();
    }
}

fn read(path: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Io(format!("cannot read {path}: {e}")))
}

/// Send the soft key to the one unit on USB MIDI and wait for it to come
/// back in download mode.
fn download_mode() -> Result<(), Error> {
    if !find_devices().is_empty() {
        println!("A unit is already in download mode");
        return Ok(());
    }
    let units = crate::midi::find_units();
    let unit = match units.len() {
        0 => return Err(Error::Refused("no FM-1 or SMK-37 found on USB MIDI: is it switched on and connected?".into())),
        1 => &units[0],
        _ => return Err(Error::Refused("more than one unit on USB MIDI: connect only one".into())),
    };
    unit.soft_key().map_err(Error::Io)?;
    println!("Sent the {} the download-mode message over USB MIDI; waiting for it", unit.name);
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Some((p, d)) = find_devices().into_iter().next() {
            println!("In download mode: {p}\t{d}");
            return Ok(());
        }
    }
    Err(Error::Refused(format!(
        "the {} did not come back in download mode within 20 s: its firmware may not know the soft key; \
         use its keys instead (see the window's HOW TO CONNECT)",
        unit.name
    )))
}

/// The unit to use: --device, or the one unit in download mode.
fn pick_device(device: Option<String>) -> Result<String, Error> {
    if let Some(d) = device {
        return Ok(d);
    }
    let devs = find_devices();
    match devs.len() {
        0 if !crate::midi::find_units().is_empty() => {
            Err(Error::Refused("the unit is running, not in download mode: `m-flasher download-mode` switches it".into()))
        }
        0 => Err(Error::Refused("no unit in download mode found (a USB disk \"WL82 UBOOT1.00\")".into())),
        1 => Ok(devs[0].0.clone()),
        _ => Err(Error::Refused("more than one unit in download mode: connect only one, or pass --device".into())),
    }
}

/// A hex number, with or without 0x.
fn parse_hex(what: &str, s: &str) -> Result<u32, Error> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u32::from_str_radix(digits, 16).map_err(|_| Error::Refused(format!("{what}: \"{s}\" is not a hex number")))
}

/// What `run-ram` is to do, checked.
pub(crate) struct RamJob {
    image: Vec<u8>,
    addr: u32,
    calls: Vec<u16>,
    /// (address, length, file)
    read: Option<(u32, usize, PathBuf)>,
}

/// Largest `run-ram --read`: more than any memory the chip has.
const MAX_RAM_READ: usize = 16 << 20;

impl RamJob {
    pub(crate) fn new(image: &[u8], addr: Option<&str>, calls: &[String], read: Option<&str>, output: Option<&str>) -> Result<Self, Error> {
        let addr = parse_hex("--addr", addr.ok_or_else(|| Error::Refused(format!("run-ram needs --addr\n{RUN_RAM_HELP}")))?)?;
        crate::jl::check_ram_range(addr, image.len()).map_err(Error::Refused)?;
        let calls = calls
            .iter()
            .map(|c| {
                let op = parse_hex("--call", c)?;
                let op = u16::try_from(op).map_err(|_| Error::Refused(format!("--call: \"{c}\" is more than 16 bits")))?;
                if crate::jl::NOT_CALLABLE.contains(&op) {
                    return Err(Error::Refused(format!("--call: 0x{op:04X} erases or writes flash or memory: not sent")));
                }
                Ok(op)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let read = match (read, output) {
            (None, None) => None,
            (Some(_), None) => return Err(Error::Refused("--read needs -o OUT, the file to write".into())),
            (None, Some(_)) => return Err(Error::Refused("-o is for --read".into())),
            (Some(spec), Some(out)) => {
                let (a, n) = spec.split_once(':').ok_or_else(|| Error::Refused(format!("--read: \"{spec}\" is not ADDR:LEN")))?;
                let a = parse_hex("--read address", a)?;
                let n = match n.strip_prefix("0x").or_else(|| n.strip_prefix("0X")) {
                    Some(h) => usize::from_str_radix(h, 16),
                    None => n.parse(),
                }
                .map_err(|_| Error::Refused(format!("--read: \"{n}\" is not a length")))?;
                if n == 0 || n > MAX_RAM_READ {
                    return Err(Error::Refused(format!("--read: the length must be 1 to {MAX_RAM_READ} bytes")));
                }
                if a as u64 + n as u64 > 1 << 32 {
                    return Err(Error::Refused(format!("--read: 0x{a:08X} + {n} bytes is past the end of the address space")));
                }
                Some((a, n, PathBuf::from(out)))
            }
        };
        Ok(RamJob { image: image.to_vec(), addr, calls, read })
    }

    pub(crate) fn run<S: crate::scsi::Scsi>(&self, c: &mut Connection<S>, log: &mut dyn FnMut(String)) -> Result<(), Error> {
        log(format!("Uploading {} bytes to 0x{:08X} and starting them", self.image.len(), self.addr));
        c.run_ram(self.addr, &self.image)?;
        for &op in &self.calls {
            let r = c.call_program(op)?;
            log(format!("0x{op:04X}: {}", r.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")));
        }
        if let Some((a, n, file)) = &self.read {
            log(format!("Reading {n} bytes at 0x{a:08X}"));
            let data = c.read_ram(*a, *n, &mut |done| progress(done, *n))?;
            std::fs::write(file, &data).map_err(|e| Error::Io(format!("cannot write {}: {e}", file.display())))?;
            log(format!("Wrote {}", file.display()));
        }
        let (retried, last) = crate::jl::Flash::retries(c);
        if retried > 0 {
            log(format!("{retried} transfers were sent again (last: {last})"));
        }
        log("The program is still running; switch the unit off and on to return it to normal".into());
        Ok(())
    }
}

pub fn run(args: Vec<String>) -> i32 {
    match main(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", match &e {
                Error::Refused(s) => format!("Refused: {s}"),
                other => other.to_string(),
            });
            1
        }
    }
}

fn main(args: Vec<String>) -> Result<(), Error> {
    let (mut pos, mut device, mut board, mut backup_dir, mut yes) = (vec![], None, None, default_backup_dir(), false);
    let (mut with_boot, mut wipe, mut stock, mut file) = (false, false, None, None);
    let (mut ram_addr, mut calls, mut read_spec, mut output) = (None, vec![], None, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| Error::Refused(format!("{name} needs a value\n{USAGE}")));
        match a.as_str() {
            "--device" => device = Some(val("--device")?),
            "--board" => board = Some(val("--board")?),
            "--backup-dir" => backup_dir = PathBuf::from(val("--backup-dir")?),
            "--stock" => stock = Some(val("--stock")?),
            "--file" => file = Some(val("--file")?),
            "--addr" => ram_addr = Some(val("--addr")?),
            "--call" => calls.push(val("--call")?),
            "--read" => read_spec = Some(val("--read")?),
            "-o" | "--output" => output = Some(val("-o")?),
            "--yes" => yes = true,
            "--with-bootloader" => with_boot = true,
            "--wipe-settings" => wipe = true,
            "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{}", if pos.first().map(String::as_str) == Some("run-ram") { RUN_RAM_HELP } else { USAGE });
                return Ok(());
            }
            s if s.starts_with("--") => return Err(Error::Refused(format!("unknown option {s}\n{USAGE}"))),
            _ => pos.push(a),
        }
    }
    let cmd = pos.first().cloned().unwrap_or_default();
    if cmd == "find" {
        for (p, d) in find_devices() {
            println!("{p}\t{d}");
        }
        for u in crate::midi::find_units() {
            println!("midi\t{} (running; download-mode switches it)", u.name);
        }
        return Ok(());
    }
    if cmd == "download-mode" {
        return download_mode();
    }
    if !matches!(cmd.as_str(), "info" | "backup" | "restore" | "firmware" | "bootloader" | "run-ram") {
        return Err(Error::Refused(USAGE.into()));
    }
    if cmd != "run-ram" && (ram_addr.is_some() || !calls.is_empty() || read_spec.is_some() || output.is_some()) {
        return Err(Error::Refused(format!("--addr, --call, --read and -o are for run-ram\n{USAGE}")));
    }
    // checked before the unit is touched
    let ram = if cmd == "run-ram" {
        let file = pos.get(1).ok_or_else(|| Error::Refused(RUN_RAM_HELP.into()))?;
        Some(RamJob::new(&read(file)?, ram_addr.as_deref(), &calls, read_spec.as_deref(), output.as_deref())?)
    } else {
        None
    };
    let path = pick_device(device)?;
    println!("Connecting to {path}");
    let mut log = |s: String| println!("{s}");
    if let Some(job) = ram {
        let mut c = Connection::open(&path, Device::open(&path)?)?;
        return job.run(&mut c, &mut log);
    }
    let conn = Connection::start(&path, Device::open(&path)?, &mut log)?;
    let mut s = Session::open(conn, backup_dir, &mut log, &mut progress)?;
    if let Some(b) = board {
        s.board_override = Some(board_from_key(&b).ok_or_else(|| Error::Refused(format!("unknown board {b}")))?);
    }
    s.show_on_unit();
    for (k, v) in s.info() {
        println!("  {k:11} {v}");
    }
    for (_, strong, t) in &s.analysis.evidence {
        println!("    {} {t}", if *strong { "*" } else { "-" });
    }
    let plan = match cmd.as_str() {
        "info" => return Ok(()),
        "backup" => {
            let p = s.save_backup(pos.get(1).map(|d| std::path::Path::new(d)))?;
            println!("Backup saved: {}", p.display());
            return Ok(());
        }
        "restore" => {
            let f = pos.get(1).ok_or_else(|| Error::Refused(USAGE.into()))?;
            s.plan_restore(&read(f)?, crate::ops::backup_unit_id(std::path::Path::new(f)).as_deref())?
        }
        "firmware" => s.plan_firmware(&read(pos.get(1).ok_or_else(|| Error::Refused(USAGE.into()))?)?, with_boot, wipe)?,
        _ => match (stock, file) {
            (Some(p), _) => s.plan_bootloader(None, Some(&read(&p)?))?,
            (_, Some(f)) => s.plan_bootloader(Some(&read(&f)?), None)?,
            _ => s.plan_bootloader(None, None)?,
        },
    };
    confirm_and_apply(&mut s, &plan, yes)
}

fn confirm_and_apply<F: crate::jl::Flash>(s: &mut Session<F>, plan: &Plan, yes: bool) -> Result<(), Error> {
    println!("\n{}", plan.describe());
    for w in &plan.warnings {
        println!("WARNING: {w}");
    }
    if plan.sectors.is_empty() {
        return Ok(());
    }
    if !yes {
        print!("Type \"yes\" to write: ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if answer.trim() != "yes" {
            println!("Nothing written.");
            return Ok(());
        }
    }
    let total = plan.sectors.len() * crate::formats::SECTOR;
    let mut done = 0;
    s.apply(plan, &mut |e| {
        if let crate::ops::Event::Verify(a) = e {
            done = done.max(a);
            let _ = total;
        }
    }, &mut |m| println!("{m}"))
}
