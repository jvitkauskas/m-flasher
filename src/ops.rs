//! The operations, with their safety rules.
//!
//! Every session starts by reading the whole flash twice; the two reads must
//! match. Each change is planned against that image: a target image and the
//! address ranges the operation may touch. Only the 4 KiB sectors that differ
//! inside those ranges are erased and written, each run is read back and
//! compared (and rewritten once on a mismatch), and the image is saved as a
//! backup before the first write of a session.

use crate::boards::{Analysis, BOARDS, Board, CHIP_KEY, FLASH_SIZE};
use crate::formats::{
    AppStatus, BOOT_AREA, Package, SECTOR, app_files_status, crc32, app_table, bankcb_check, bootloader_identity,
    build_boot_area, stored_bootloader,
};
use crate::jl::{Flash, Show};
use crate::scsi::ScsiError;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Error {
    /// An operation the tool will not do, with the reason.
    Refused(String),
    Usb(ScsiError),
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Refused(s) | Error::Io(s) => f.write_str(s),
            Error::Usb(e) => write!(f, "{e}"),
        }
    }
}

impl From<ScsiError> for Error {
    fn from(e: ScsiError) -> Self {
        Error::Usb(e)
    }
}

fn refuse<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error::Refused(s.into()))
}

pub fn bundled_bootloader(board: &Board) -> Result<(&'static [u8], String), Error> {
    let boot: &'static [u8] = match board.key {
        "fm1" => include_bytes!("../data/bootloader-fm1.boot"),
        _ => include_bytes!("../data/bootloader-smk37.boot"),
    };
    match bootloader_identity(boot) {
        Some((tag, version)) if tag == board.tag => Ok((boot, version)),
        _ => refuse(format!("the bundled bootloader for the {} is damaged", board.name)),
    }
}

pub fn ranges_text(ranges: &[(usize, usize)]) -> String {
    ranges.iter().map(|(a, b)| format!("0x{a:05X}-0x{:05X}", b - 1)).collect::<Vec<_>>().join(", ")
}

/// A change: the target image, the ranges it may touch, the sectors that differ
/// and what the user should know before confirming.
#[derive(Clone)]
pub struct Plan {
    pub title: String,
    pub target: Vec<u8>,
    #[allow(dead_code)] // checked when planning; the tests read it
    pub allowed: Vec<(usize, usize)>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub sectors: Vec<usize>,
}

impl Plan {
    pub fn new(title: String, current: &[u8], target: Vec<u8>, allowed: Vec<(usize, usize)>,
               notes: Vec<String>, warnings: Vec<String>) -> Result<Plan, Error> {
        if target.len() != current.len() {
            return refuse("internal error: image size mismatch");
        }
        let mut sectors = vec![];
        for a in (0..current.len()).step_by(SECTOR) {
            if current[a..a + SECTOR] != target[a..a + SECTOR] {
                if !allowed.iter().any(|&(s, e)| s <= a && a + SECTOR <= e) {
                    return refuse(format!("internal error: the change at 0x{a:05X} is outside the allowed ranges ({})",
                                          ranges_text(&allowed)));
                }
                sectors.push(a);
            }
        }
        Ok(Plan { title, target, allowed, notes, warnings, sectors })
    }

    pub fn runs(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = vec![];
        for &a in &self.sectors {
            match out.last_mut() {
                Some(r) if r.1 == a => r.1 = a + SECTOR,
                _ => out.push((a, a + SECTOR)),
            }
        }
        out
    }

    pub fn describe(&self) -> String {
        let mut lines = vec![self.title.clone()];
        if self.sectors.is_empty() {
            lines.push("Nothing to write: the flash already holds this.".into());
        } else {
            lines.push(format!("Writes {} sectors ({} KiB): {}", self.sectors.len(), self.sectors.len() * 4,
                               ranges_text(&self.runs())));
        }
        lines.extend(self.notes.iter().cloned());
        lines.join("\n")
    }
}

/// Progress of a write: what is happening and the address done up to.
pub enum Event {
    Backup,
    Erase(usize),
    Write(usize),
    Verify(usize),
}

/// A connected unit: its flash image as read, and the operations.
pub struct Session<F: Flash> {
    pub dev: F,
    pub flash: Vec<u8>,
    pub analysis: Analysis,
    pub backup_dir: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub board_override: Option<&'static Board>,
    /// The flash chip's factory-unique ID (our loader only): which unit this is.
    pub unit_id: Option<String>,
    /// The unit's own display is showing our screens.
    pub display: bool,
}

// the unit's display: colours (RGB565) and the screens' text (its font has
// A-Z, 0-9 and - . / : < > + ? only)
const MINT: u16 = 0x7F57;
const LAV: u16 = 0xB51C;
const AMBER: u16 = 0xFDA0;
const WHITE: u16 = 0xFFFF;

fn screen_text(s: &str) -> String {
    s.to_uppercase().chars().map(|c| if c.is_ascii_alphanumeric() || " -./:<>+?".contains(c) { c } else { ' ' }).collect()
}

impl<F: Flash> Session<F> {
    pub fn open(mut dev: F, backup_dir: PathBuf, log: &mut dyn FnMut(String),
                progress: &mut dyn FnMut(usize, usize)) -> Result<Self, Error> {
        if dev.chip_key() != CHIP_KEY {
            log(format!("Note: chip key 0x{:04X} (0x{CHIP_KEY:04X} expected)", dev.chip_key()));
        }
        log("Reading the flash".into());
        let started = std::time::Instant::now();
        let a = dev.read(0, FLASH_SIZE, &mut |d| progress(d, FLASH_SIZE))?;
        let rate = FLASH_SIZE as f64 / 1024.0 / started.elapsed().as_secs_f64().max(1e-3);
        log(format!("Read in {:.1} s ({rate:.0} KB/s)", started.elapsed().as_secs_f64()));
        if rate < 400.0 {
            // Some USB controllers (seen: AMD's chipset one) collect only a few of
            // the unit's 64-byte packets per millisecond; others take 3-4 times more.
            log("Tip: this USB port is slow for the unit. A port on another USB controller can be \
                 3-4 times faster, often a rear one next to the video outputs (on the CPU's own controller)."
                .into());
        }
        // The read is checked against the loader's CRC32 of the whole flash,
        // computed on the chip in under a second (a loader without it: read again).
        match dev.crc32(0, FLASH_SIZE)? {
            Some(c) if c == crc32(&a) => log("Checked: the chip's CRC32 of the flash matches the read".into()),
            check => {
                if check.is_some() {
                    log("The chip's CRC32 does not match the read: reading it again".into());
                } else {
                    log("Reading it again to compare".into());
                }
                let b = dev.read(0, FLASH_SIZE, &mut |d| progress(d, FLASH_SIZE))?;
                if a != b || check.is_some_and(|c| c != crc32(&b)) {
                    return refuse("two reads of the flash differ: the USB connection is unreliable. Nothing was written. \
                                   Try another cable or port, without a hub.");
                }
            }
        }
        let analysis = Analysis::new(&a, Some(dev.flash_id()), dev.chip_key());
        let unit_id = dev.unique_id();
        Ok(Session { dev, flash: a, analysis, backup_dir, backup_path: None, board_override: None, unit_id, display: false })
    }

    pub fn board(&self) -> Option<&'static Board> {
        self.board_override.or(self.analysis.board)
    }

    fn need_board(&self) -> Result<&'static Board, Error> {
        self.board().ok_or_else(|| Error::Refused("the model could not be identified: choose it first".into()))
    }

    /// (label, value) pairs for display and for the backup's .json.
    pub fn info(&self) -> Vec<(&'static str, String)> {
        vec![
            ("model", self.board_override.map(|b| b.name.to_string()).unwrap_or_else(|| self.analysis.model_text())),
            ("bootloader", self.analysis.bootloader_text.clone()),
            ("firmware", self.analysis.firmware_text.clone()),
            ("app", self.analysis.app_status.text().to_string()),
            ("chip key", format!("0x{:04X}", self.dev.chip_key())),
            ("flash", format!("0x{:06X}", self.dev.flash_id())),
            ("unit id", self.unit_id.clone().unwrap_or_else(|| "–".into())),
            ("loader", self.dev.loader()),
        ]
    }

    /// Show our screens on the unit's own display, if the loader can and the
    /// model (which says where the display is wired) is known.
    pub fn show_on_unit(&mut self) {
        let Some(board) = self.board() else { return };
        if self.board_override.is_none() && !self.analysis.certain {
            return;
        }
        self.dev.show(Show::On(if board.key == "fm1" { 1 } else { 2 }));
        self.display = true;
        let firmware = screen_text(&self.analysis.firmware_text);
        self.screen(&[(1, " M-FLASHER", MINT), (3, " CONNECTED", WHITE), (5, &format!(" {}", screen_text(board.name)), LAV),
                      (6, &format!(" {firmware}"), LAV), (12, " READY", MINT)]);
    }

    fn screen(&mut self, lines: &[(u8, &str, u16)]) {
        if !self.display {
            return;
        }
        for &(row, text, color) in lines {
            self.dev.show(Show::Text(row, text, color));
        }
    }

    pub fn save_backup(&mut self, dir: Option<&Path>) -> Result<PathBuf, Error> {
        let dir = dir.map(Path::to_path_buf).unwrap_or_else(|| self.backup_dir.clone());
        create_dirs(&dir).map_err(|e| Error::Io(format!("cannot create {}: {e}", dir.display())))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let model = self.board().map(|b| b.key).unwrap_or("unknown");
        // two backups in the same second: the second gets -2 (and so on)
        let path = (1..)
            .map(|i| dir.join(if i == 1 { format!("{model}-{stamp}.bin") } else { format!("{model}-{stamp}-{i}.bin") }))
            .find(|p| !p.exists() && !p.with_extension("json").exists())
            .unwrap();
        std::fs::write(&path, &self.flash).map_err(|e| Error::Io(format!("cannot write {}: {e}", path.display())))?;
        let mut json = String::from("{\n");
        for (k, v) in self.info() {
            json += &format!("  \"{k}\": \"{}\",\n", v.replace('\\', "\\\\").replace('"', "\\\""));
        }
        let sha: String = Sha256::digest(&self.flash).iter().map(|b| format!("{b:02x}")).collect();
        json += &format!("  \"sha256\": \"{sha}\",\n");
        json += &format!("  \"saved\": \"{stamp}\",\n  \"tool\": \"m-flasher {}\"\n}}\n", env!("CARGO_PKG_VERSION"));
        std::fs::write(path.with_extension("json"), json).map_err(|e| Error::Io(e.to_string()))?;
        give_to_sudo_user(&path);
        give_to_sudo_user(&path.with_extension("json"));
        self.backup_path = Some(path.clone());
        Ok(path)
    }

    /// backup_unit: the unit ID saved with the backup, if any. When both it and
    /// this unit's are known, they decide whether it is this unit's backup.
    pub fn plan_restore(&self, data: &[u8], backup_unit: Option<&str>) -> Result<Plan, Error> {
        if data.len() != FLASH_SIZE {
            return refuse(format!("not a full backup: {} bytes, {FLASH_SIZE} expected", data.len()));
        }
        let backup = Analysis::new(data, None, CHIP_KEY);
        let board = self.board();
        if let (Some(a), Some(b)) = (backup.board, board) {
            if a.key != b.key {
                return refuse(format!("this backup is from a {}, the connected unit is a {}", a.name, b.name));
            }
        }
        let factory: &[(usize, usize)] = board.map(|b| b.factory).unwrap_or(&[(0xFF000, FLASH_SIZE)]);
        let same_unit = match (backup_unit, self.unit_id.as_deref()) {
            (Some(b), Some(u)) => b == u,
            _ => factory.iter().all(|&(a, b)| data[a..b] == self.flash[a..b] || self.flash[a..b].iter().all(|&x| x == 0xFF)),
        };
        let (mut notes, mut warnings) = (vec![], vec![]);
        let allowed = if same_unit {
            notes.push("The whole flash is restored, the bootloader included.".into());
            for &(a, b) in factory {
                if data[a..b] != self.flash[a..b] {
                    warnings.push(format!("This unit's factory data at {} is blank; the backup's is restored. \
                                           Use only a backup of this unit.", ranges_text(&[(a, b)])));
                }
            }
            vec![(0, FLASH_SIZE)]
        } else {
            let mut fs = factory.to_vec();
            fs.sort();
            let (mut allowed, mut at) = (vec![], 0);
            for (a, b) in fs {
                if a > at {
                    allowed.push((at, a));
                }
                at = b;
            }
            if at < FLASH_SIZE {
                allowed.push((at, FLASH_SIZE));
            }
            warnings.push(format!("This backup {} another unit: its factory data ({}: calibration, Bluetooth address) is \
                                   not restored, this unit keeps its own.",
                                  if backup_unit.is_some() && self.unit_id.is_some() { "is from" } else { "seems to come from" },
                                  ranges_text(factory)));
            allowed
        };
        let mut target = self.flash.clone();
        for &(a, b) in &allowed {
            target[a..b].copy_from_slice(&data[a..b]);
        }
        Plan::new("Restore a backup".into(), &self.flash, target, allowed, notes, warnings)
    }

    pub fn plan_firmware(&self, raw: &[u8], with_bootloader: bool, wipe_settings: bool) -> Result<Plan, Error> {
        let pkg = Package::parse(raw).map_err(|e| Error::Refused(e.0))?;
        let board = self.need_board()?;
        if !board.models.contains(&pkg.model()) {
            return refuse(format!("this package is for the {}, the connected unit is a {}", pkg.model(), board.name));
        }
        if pkg.flash.len() > FLASH_SIZE {
            return refuse("the package is larger than the flash");
        }
        let key = self.dev.chip_key();
        let mut padded = pkg.flash.clone();
        padded.resize(FLASH_SIZE, 0xFF);
        if app_files_status(&padded, key) != AppStatus::Ok {
            return refuse(format!("the package's firmware does not decrypt with this unit's chip key (0x{key:04X}): \
                                   writing it would not start"));
        }
        let (mut notes, mut warnings) = (vec![], vec![]);
        if let Some(m) = &self.analysis.model_name {
            if m.starts_with("SMK-37 ") && m != pkg.model() {
                warnings.push(format!("The unit runs {m} firmware; this package is for the {}. The unit will then \
                                       report itself as that model.", pkg.model()));
            }
        }
        let regions = |img: &[u8]| -> Vec<(String, u32, u32)> {
            let mut v: Vec<_> = app_table(img, key).into_iter().filter(|e| e.attr == 0x12 || e.attr == 0x92)
                .map(|e| (e.name, e.addr, e.len)).collect();
            v.sort();
            v
        };
        let old = regions(&self.flash);
        if !old.is_empty() && old != regions(&padded) {
            warnings.push(format!("The installed firmware ({}) keeps its data in other places than {}; what it leaves \
                                   behind can confuse the new firmware (wrong presets, no sound). To go back to a \
                                   firmware cleanly, restore a backup made while it was installed.",
                                  self.analysis.firmware_text, pkg.identity));
        }
        let start = if with_bootloader { 0 } else { BOOT_AREA };
        let end = pkg.flash.len();
        let mut target = self.flash.clone();
        target[start..end].copy_from_slice(&pkg.flash[start..end]);
        let mut allowed = vec![(start, end)];
        notes.push(format!("Installs {} ({}).", pkg.identity, ranges_text(&allowed)));
        if with_bootloader {
            notes.push("Also puts back the package's (stock) bootloader.".into());
        }
        if wipe_settings {
            for e in app_table(&padded, key) {
                let (a, n) = (e.addr as usize, e.len as usize);
                if (e.name == "VM" || e.name == "BTIF") && (e.attr == 0x12 || e.attr == 0x92) && n > 0
                    && a % SECTOR == 0 && a + n <= FLASH_SIZE {
                    if board.factory.iter().any(|&(fa, fb)| fa < a + n && a < fb) {
                        return refuse(format!("the package declares {} over factory data", e.name));
                    }
                    target[a..a + n].fill(0xFF);
                    allowed.push((a, a + n));
                    notes.push(format!("Erases {} (0x{a:05X}-0x{:05X}): {}", e.name, a + n - 1,
                                       if e.name == "VM" { "settings" } else { "Bluetooth pairings" }));
                }
            }
        }
        Plan::new(format!("Install firmware {}", pkg.identity), &self.flash, target, allowed, notes, warnings)
    }

    /// Our bundled bootloader (`boot` None), a given uboot.boot, or the stock one
    /// from a package's boot area.
    pub fn plan_bootloader(&self, boot: Option<&[u8]>, package: Option<&[u8]>) -> Result<Plan, Error> {
        let board = self.need_board()?;
        let (boot, title): (Vec<u8>, String) = if let Some(raw) = package {
            let pkg = Package::parse(raw).map_err(|e| Error::Refused(e.0))?;
            if !board.models.contains(&pkg.model()) {
                return refuse(format!("this package is for the {}, the connected unit is a {}", pkg.model(), board.name));
            }
            let b = stored_bootloader(&pkg.flash).ok_or_else(|| Error::Refused("the package has no readable bootloader".into()))?;
            (b, format!("Put back the stock bootloader (from {})", pkg.identity))
        } else if let Some(b) = boot {
            bankcb_check(b).map_err(|e| Error::Refused(e.0))?;
            let ident = bootloader_identity(b);
            if let Some((tag, _)) = &ident {
                if tag != board.tag {
                    return refuse(format!("this bootloader is built for \"{tag}\", the unit is a {}", board.name));
                }
            }
            let v = ident.map(|i| i.1).unwrap_or_else(|| "unknown".into());
            (b.to_vec(), format!("Install the bootloader {v} for the {}", board.name))
        } else {
            let (b, v) = bundled_bootloader(board)?;
            (b.to_vec(), format!("Install m-bootloader {v} for the {}", board.name))
        };
        let area = build_boot_area(&self.flash, &boot)
            .map_err(|e| Error::Refused(format!("{}; install firmware with its bootloader instead", e.0)))?;
        let mut target = area;
        target.extend_from_slice(&self.flash[BOOT_AREA..]);
        Plan::new(title, &self.flash, target, vec![(0, BOOT_AREA)],
                  vec!["Only the boot area (0x00000-0x03FFF) is written; the unit's own configuration in it is kept.".into()],
                  vec![])
    }

    pub fn apply(&mut self, plan: &Plan, event: &mut dyn FnMut(Event), log: &mut dyn FnMut(String)) -> Result<(), Error> {
        if plan.sectors.is_empty() {
            log("Nothing to write".into());
            return Ok(());
        }
        if self.backup_path.is_none() {
            event(Event::Backup);
            let p = self.save_backup(None)?;
            log(format!("Backup saved: {}", p.display()));
        }
        let what = if plan.title.starts_with("Install firmware") { " INSTALL FIRMWARE" }
            else if plan.title.starts_with("Restore") { " RESTORE BACKUP" } else { " BOOTLOADER" };
        let total = plan.sectors.len();
        self.screen(&[(3, what, WHITE), (5, " WRITING", WHITE), (6, "", WHITE), (12, " DO NOT SWITCH OFF", AMBER)]);
        let (mut done, mut shown) = (0usize, usize::MAX);
        for (a, b) in plan.runs() {
            let want = &plan.target[a..b];
            let mut attempt = 1;
            loop {
                log(format!("Writing 0x{a:05X}-0x{:05X}{}", b - 1, if attempt == 2 { " (again)" } else { "" }));
                event(Event::Erase(a));
                self.dev.erase(a, b - a)?;
                // sector by sector, so that the unit's display can follow
                for s in (a..b).step_by(SECTOR) {
                    self.dev.program(s, &plan.target[s..s + SECTOR], &mut |n| event(Event::Write(s + n)))?;
                    let pct = 100 * (done + (s + SECTOR - a) / SECTOR) / total;
                    if self.display && pct / 2 != shown / 2 {
                        shown = pct;
                        self.dev.show(Show::Bar(pct as u8, MINT));
                        let t = format!(" {pct:3}%  {}/{total}", done + (s + SECTOR - a) / SECTOR);
                        self.dev.show(Show::Text(13, &t, MINT));
                    }
                }
                // verify: the chip's CRC32 of what is now there (ours), or read it all back
                let ok = match self.dev.crc32(a, b - a)? {
                    Some(c) => {
                        event(Event::Verify(b));
                        c == crc32(want)
                    }
                    None => self.dev.read(a, b - a, &mut |n| event(Event::Verify(a + n)))? == want,
                };
                if ok {
                    break;
                }
                let got = self.dev.read(a, b - a, &mut |_| {})?;
                if attempt == 2 {
                    self.flash[a..b].copy_from_slice(&got);
                    return refuse(format!("0x{a:05X}-0x{:05X} does not read back as written. Do not switch the unit off: \
                                           try again, with another cable or port. Your backup: {}",
                                          b - 1, self.backup_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()));
                }
                attempt += 1;
            }
            self.flash[a..b].copy_from_slice(want);
            done += (b - a) / SECTOR;
        }
        self.analysis = Analysis::new(&self.flash, Some(self.dev.flash_id()), self.dev.chip_key());
        if self.display {
            self.dev.show(Show::Bar(100, MINT));
        }
        self.screen(&[(5, " DONE - VERIFIED", MINT), (13, &format!(" {total} SECTORS OK"), MINT), (12, " SWITCH OFF AND ON", WHITE)]);
        log(if self.dev.loader().starts_with("M-Flasher") {
            "Done: everything written is verified (the chip's CRC32 matches)".into()
        } else {
            "Done: everything written was read back and matches".into()
        });
        Ok(())
    }
}

/// The unit ID saved in a backup's .json, if any.
pub fn backup_unit_id(backup: &Path) -> Option<String> {
    let json = std::fs::read_to_string(backup.with_extension("json")).ok()?;
    let id = json.split("\"unit id\": \"").nth(1)?.split('"').next()?;
    (id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
}

/// create_dir_all, the new folders given to the user who ran sudo.
pub fn create_dirs(dir: &Path) -> std::io::Result<()> {
    let new: Vec<PathBuf> = dir.ancestors().take_while(|d| !d.as_os_str().is_empty() && !d.exists())
        .map(Path::to_path_buf).collect();
    std::fs::create_dir_all(dir)?;
    new.iter().for_each(|d| give_to_sudo_user(d));
    Ok(())
}

/// Run with sudo (macOS needs it), the files the app creates still belong to
/// the user who ran it.
fn give_to_sudo_user(_path: &Path) {
    #[cfg(unix)]
    {
        let id = |v| std::env::var(v).ok().and_then(|s| s.parse::<u32>().ok());
        if let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) {
            let _ = std::os::unix::fs::chown(_path, Some(uid), Some(gid));
        }
    }
}

/// The model by its key (for the command line's --board).
pub fn board_from_key(key: &str) -> Option<&'static Board> {
    BOARDS.iter().copied().find(|b| b.key == key)
}
