//! Command-line mode (the same operations as the window):
//!
//! ```text
//! m-flasher find | download-mode | info | backup [DIR] | restore BACKUP.bin
//!           firmware PACKAGE.fwsc [--with-bootloader] [--wipe-settings]
//!           bootloader [--stock PACKAGE.fwsc | --file UBOOT.boot]
//! options:  --device PATH  --board fm1|smk37  --backup-dir DIR  --yes
//! ```

use crate::jl::Connection;
use crate::ops::{Error, Plan, Session, board_from_key};
use crate::scsi::{Device, find_devices};
use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "usage: m-flasher [find | download-mode | info | backup [DIR] | restore BACKUP.bin |
                 firmware PACKAGE.fwsc [--with-bootloader] [--wipe-settings] |
                 bootloader [--stock PACKAGE.fwsc | --file UBOOT.boot]]
       options: --device PATH  --board fm1|smk37  --backup-dir DIR  --yes
       without arguments: the window";

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
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| Error::Refused(format!("{name} needs a value\n{USAGE}")));
        match a.as_str() {
            "--device" => device = Some(val("--device")?),
            "--board" => board = Some(val("--board")?),
            "--backup-dir" => backup_dir = PathBuf::from(val("--backup-dir")?),
            "--stock" => stock = Some(val("--stock")?),
            "--file" => file = Some(val("--file")?),
            "--yes" => yes = true,
            "--with-bootloader" => with_boot = true,
            "--wipe-settings" => wipe = true,
            "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
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
    if !matches!(cmd.as_str(), "info" | "backup" | "restore" | "firmware" | "bootloader") {
        return Err(Error::Refused(USAGE.into()));
    }
    let path = match device {
        Some(d) => d,
        None => {
            let devs = find_devices();
            match devs.len() {
                0 if !crate::midi::find_units().is_empty() => {
                    return Err(Error::Refused(
                        "the unit is running, not in download mode: `m-flasher download-mode` switches it".into(),
                    ));
                }
                0 => return Err(Error::Refused("no unit in download mode found (a USB disk \"WL82 UBOOT1.00\")".into())),
                1 => devs[0].0.clone(),
                _ => return Err(Error::Refused("more than one unit in download mode: connect only one, or pass --device".into())),
            }
        }
    };
    println!("Connecting to {path}");
    let mut log = |s: String| println!("{s}");
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
