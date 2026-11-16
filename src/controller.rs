//! The window's logic, apart from the drawing: finds the unit, connects, plans
//! the selected operation and writes it with progress per sector, in
//! background threads. Everything the window shows is in `Shared`, which the
//! window copies each frame; the threads change it and ask for a repaint.

use crate::boards::{BOARDS, CHIP_KEY, flash_text};
use crate::cli::default_backup_dir;
use crate::formats::SECTOR;
use crate::jl::{Connection, Flash};
use crate::ops::{Error, Event, Plan, Session, bundled_bootloader};
use crate::scsi::{Device, ScsiError, find_devices};
use crate::sectormap::{Region, sector_map};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Unit = Session<Connection<Device>>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Phase {
    Waiting,
    Connecting,
    Ready,
    Working,
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Backup,
    Restore,
    Firmware,
    Bootloader,
}

// The window installs the bundled m-bootloader only (Device: INSTALL
// M-BOOTLOADER); the stock and .boot sources stay for the planner and the
// command line.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BootSource {
    Open,
    Stock,
    File,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mark {
    Change,
    Now,
    Written,
    Verified,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Stage {
    Idle,
    Active,
    Done,
}

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FileKind {
    Restore,
    Firmware,
    Stock,
    BootFile,
}

#[derive(Clone, PartialEq, Debug)]
pub enum PlanKind {
    Backup,
    Write,
    /// Something to choose first (the text says what).
    Need,
    Refused,
}

#[derive(Clone, Debug)]
pub struct PlanView {
    pub kind: PlanKind,
    pub title: String,
    pub count: usize,
    pub ranges: String,
    pub short: String,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub ready: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlertKind {
    Done,
    Refused,
    Error,
}

#[derive(Clone, Debug)]
pub struct DeviceView {
    pub model: String,
    pub certain: bool,
    pub firmware: String,
    pub app: String,
    pub app_ok: bool,
    pub bootloader: String,
    /// m-bootloader is the unit's bootloader (its rescue mode is there).
    pub boot_open: bool,
    pub flash: String,
    /// The chip with its name: "Puya P25Q80H · 0x856014 (old)".
    pub flash_label: String,
    pub chip_key: String,
    pub chip_key_ok: bool,
    pub evidence: Vec<String>,
}

/// A firmware of the library, as the Library tab lists it.
#[derive(Clone, Debug)]
pub struct LibItem {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Author and licence.
    pub byline: String,
    pub kinds: String,
    /// "old": the FM-1's older flash chip only; "new"; "any".
    pub chip: String,
    pub beta: bool,
    /// On this computer and checked.
    pub saved: bool,
    /// Installed with its author's own installer, not from a package.
    pub elsewhere: bool,
    /// Versions of it on this computer older than the list's: UPDATE.
    pub older: Vec<String>,
    /// Starred.
    pub favorite: bool,
}

#[derive(Clone, Debug)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub name: String,
    pub time: String,
    pub firmware: String,
}

/// Everything the window shows.
#[derive(Clone)]
pub struct Shared {
    pub phase: Phase,
    pub busy: bool,
    pub multiple: bool,
    pub device: Option<DeviceView>,
    pub map: Vec<Region>,
    pub marks: HashMap<usize, Mark>,
    pub stage: [Stage; 4], // read, backup, write, verify
    pub pct: f32,
    pub cur: Option<usize>,
    pub plan: Option<PlanView>,
    pub alert: Option<(AlertKind, String)>,
    pub log: Vec<String>,
    pub backups: Vec<BackupInfo>,
    pub backup_dir: PathBuf,
    pub backup_saved: Option<String>,
    pub open_version: String,
    // the user's choices
    pub mode: Mode,
    pub files: HashMap<&'static str, PathBuf>,
    /// Restore: the unit keeps its own bootloader.
    pub keep_boot: bool,
    pub boot_source: BootSource,
    /// A unit running normally, found on USB MIDI (its name), while none is in
    /// download mode: the soft key can switch it.
    pub midi: Option<String>,
    /// After a write's restart: the scan reports every MIDI port (name, taken for a unit) in `midi_ports`, even
    /// while the write's work runs. On macOS only the scan's thread sees the list change (its run loop gets
    /// CoreMIDI's notifications); a list made on another thread stays as it was.
    pub watching_restart: bool,
    pub midi_ports: Option<Vec<(String, bool)>>,
    /// Recommend m-bootloader to the user (the unit has the stock bootloader):
    /// the key that opens its recovery menu.
    pub suggest_boot: Option<&'static str>,
    /// The Library tab: the firmwares, what the list's state is, and whether it is being read.
    pub library: Vec<LibItem>,
    pub library_note: String,
    pub library_loading: bool,
    /// Library: the firmwares being downloaded (GET), by id.
    pub library_getting: Vec<String>,
    /// Installing firmware on a unit with M-VAVE's stock bootloader: m-bootloader
    /// goes first (REQ-BOOT-01), the key that opens its recovery menu.
    pub boot_first: Option<&'static str>,
    /// Step 1 (m-bootloader) is being installed: the firmware is step 2.
    pub firmware_after: bool,
    /// Step 1 is written: the window opens the firmware's review.
    pub review_next: bool,
    /// Library INSTALL with no unit in download mode: the firmware (id) to plan once one is.
    pub install_after: Option<String>,
    /// How to put the unit in download mode by hand, shown while INSTALL waits for it.
    pub how_to: Option<String>,
    /// Library INSTALL planned: the window goes back to DEVICE, its firmware form open.
    pub library_review: bool,
    /// The firmware chosen comes from the library: its name and version.
    pub library_choice: Option<String>,
}

fn file_key(k: FileKind) -> &'static str {
    match k {
        FileKind::Restore => "restore",
        FileKind::Firmware => "firmware",
        FileKind::Stock => "stock",
        FileKind::BootFile => "bootfile",
    }
}

impl Shared {
    pub fn file(&self, k: FileKind) -> Option<String> {
        self.files.get(file_key(k)).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
    }
}

#[derive(Clone)]
pub struct Controller {
    shared: Arc<Mutex<Shared>>,
    unit: Arc<Mutex<Option<Unit>>>,
    plan: Arc<Mutex<Option<Plan>>>,
    /// The unit M-Flasher had no rights to open (the system's dialog for
    /// administrator rights cancelled, Linux's udev rule missing), or on
    /// Windows could not open at all: not tried, nor asked about, again every
    /// scan, only once it is reconnected.
    declined: Arc<Mutex<Option<String>>>,
    /// The unit that last failed to connect, and how many times in a row.
    failures: Arc<Mutex<(String, u32)>>,
    /// Linux: a unit is in download mode but the SCSI generic driver isn't
    /// loaded, and the window has said so (once, until it changes).
    sg_missing: Arc<AtomicBool>,
    /// Counts the user's choices (mode, files, options, model), and the count
    /// the plan was made from: a plan from older choices is never written.
    choices: Arc<AtomicU64>,
    planned: Arc<AtomicU64>,
    /// The file dialogs open: no write starts while a file is being chosen.
    picking: Arc<AtomicU64>,
    ctx: egui::Context,
}

use eframe::egui;

/// Failed connections in a row before a unit is left alone until reconnected.
const CONNECT_TRIES: u32 = 3;

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl Shared {
    pub(crate) fn new(open_version: String, backup_dir: PathBuf) -> Shared {
        Shared {
            phase: Phase::Waiting,
            busy: false,
            watching_restart: false,
            midi_ports: None,
            multiple: false,
            device: None,
            map: vec![Region::Free; 256],
            marks: HashMap::new(),
            stage: [Stage::Idle; 4],
            pct: 0.0,
            cur: None,
            plan: None,
            alert: None,
            log: vec![],
            backups: vec![],
            backup_dir,
            backup_saved: None,
            open_version,
            mode: Mode::Firmware,
            files: HashMap::new(),
            keep_boot: false,
            boot_source: BootSource::Open,
            midi: None,
            suggest_boot: None,
            library: vec![],
            library_note: String::new(),
            library_loading: false,
            library_getting: vec![],
            boot_first: None,
            firmware_after: false,
            review_next: false,
            install_after: None,
            how_to: None,
            library_review: false,
            library_choice: None,
        }
    }

    /// Two steps (REQ-BOOT-01), step 1 chosen: m-bootloader, the firmware kept for step 2.
    pub(crate) fn begin_two_step(&mut self) -> bool {
        let ok = !self.busy && self.mode == Mode::Firmware;
        if ok {
            self.firmware_after = true;
            self.mode = Mode::Bootloader;
            self.boot_source = BootSource::Open;
        }
        ok
    }

    /// The two steps given up before step 1 was written: back to the firmware.
    pub(crate) fn drop_two_step(&mut self) -> bool {
        let was = std::mem::take(&mut self.firmware_after);
        if was && !self.busy {
            self.mode = Mode::Firmware;
        }
        was
    }

    /// Step 1 written and verified: the firmware, still chosen, is step 2,
    /// its review opened by the window. Whether this was step 1 of two.
    pub(crate) fn end_step_one(&mut self, total: usize) -> bool {
        if !(std::mem::take(&mut self.firmware_after) && self.mode == Mode::Bootloader) {
            return false;
        }
        self.mode = Mode::Firmware;
        self.phase = Phase::Ready;
        self.review_next = true;
        self.alert = Some((AlertKind::Done, format!("FM-1 bootloader is now protected with m-bootloader {}. It was \
            written and verified ({total} sectors). Next Step: Confirm firmware install.", self.open_version)));
        true
    }
}

impl Controller {
    pub fn new(ctx: egui::Context) -> Controller {
        let open_version = bundled_bootloader(BOARDS[1]).map(|(_, v)| v).unwrap_or_else(|_| "?".into());
        // shown as its release ("2.4"); the log keeps the build ("2.4-395e6cc")
        let shown_version = open_version.split('-').next().unwrap_or(&open_version).to_string();
        let backup_dir = default_backup_dir();
        let c = Controller {
            shared: Arc::new(Mutex::new(Shared::new(shown_version, backup_dir))),
            unit: Arc::new(Mutex::new(None)),
            plan: Arc::new(Mutex::new(None)),
            declined: Arc::new(Mutex::new(None)),
            failures: Arc::new(Mutex::new((String::new(), 0))),
            sg_missing: Arc::new(AtomicBool::new(false)),
            choices: Arc::new(AtomicU64::new(0)),
            planned: Arc::new(AtomicU64::new(0)),
            picking: Arc::new(AtomicU64::new(0)),
            ctx,
        };
        c.log(format!("M-Flasher {} started · m-bootloader {open_version} bundled", env!("CARGO_PKG_VERSION")));
        c.refresh_backups();
        let scanner = c.clone();
        std::thread::spawn(move || scanner.scan_loop());
        c
    }

    pub fn snapshot(&self) -> Shared {
        self.shared.lock().unwrap().clone()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        let r = f(&mut self.shared.lock().unwrap());
        self.ctx.request_repaint();
        r
    }

    pub fn log(&self, text: String) {
        let dir = self.with(|s| {
            s.log.push(format!("[{}] {text}", now()));
            let n = s.log.len();
            if n > 200 {
                s.log.drain(..n - 200);
            }
            s.backup_dir.clone()
        });
        log_to_file(&dir, &text);
    }

    /// Run f in a thread unless something is running; errors become alerts.
    fn work(&self, name: &'static str, f: impl FnOnce(&Controller) -> Result<(), Error> + Send + 'static) {
        self.work_if(name, |_| true, f);
    }

    /// As `work`, but only if `admit` agrees. It is asked with what the
    /// window shows locked, in the same step that marks the work as running:
    /// no choice can change between its answer and the start.
    fn work_if(&self, name: &'static str, admit: impl FnOnce(&Shared) -> bool,
               f: impl FnOnce(&Controller) -> Result<(), Error> + Send + 'static) {
        if !self.with(|s| !s.busy && admit(s) && { s.busy = true; true }) {
            return;
        }
        let c = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = f(&c) {
                let (kind, text) = match &e {
                    Error::Refused(t) => (AlertKind::Refused, format!("Refused: {t}")),
                    Error::Usb(u) => (AlertKind::Error, format!("USB error during {name}: {u}")),
                    Error::Io(t) => (AlertKind::Error, t.clone()),
                };
                c.log(text.clone());
                c.with(|s| {
                    if matches!(s.phase, Phase::Working | Phase::Connecting) {
                        s.phase = if s.device.is_some() { Phase::Ready } else { Phase::Waiting };
                    }
                    s.alert = Some((kind, text));
                });
                if matches!(e, Error::Usb(_)) {
                    c.drop_unit();
                }
            }
            c.with(|s| s.busy = false);
            // a choice made while this ran was not planned (one thing runs at a time)
            if c.choices.load(Ordering::SeqCst) != c.planned.load(Ordering::SeqCst)
                && c.shared.lock().unwrap().phase == Phase::Ready {
                c.replan();
            }
        });
    }

    /// Changes the user's choices, and counts the change.
    fn choose<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        self.with(|s| {
            self.choices.fetch_add(1, Ordering::SeqCst);
            f(s)
        })
    }

    // ---- connection ----

    fn scan_loop(&self) {
        loop {
            if self.shared.lock().unwrap().watching_restart {
                let ports = crate::midi::port_names();
                self.with(|s| if s.watching_restart { s.midi_ports = Some(ports) });
            }
            if !self.shared.lock().unwrap().busy {
                let devs = find_devices();
                // the scan may have waited for the USB (the helper) while a work started: it acts on nothing then
                if self.shared.lock().unwrap().busy {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                let connected = self.unit.lock().unwrap().as_ref().map(|u| u.dev.path().to_string());
                let declined = {
                    let mut d = self.declined.lock().unwrap();
                    if d.as_ref().is_some_and(|p| !devs.iter().any(|x| &x.0 == p)) {
                        *d = None;
                    }
                    let mut f = self.failures.lock().unwrap();
                    if f.1 > 0 && !devs.iter().any(|x| x.0 == f.0) {
                        *f = (String::new(), 0);
                    }
                    d.clone()
                };
                match &connected {
                    Some(p) if !devs.iter().any(|d| &d.0 == p) => {
                        self.log("The unit is gone (unplugged or switched off)".into());
                        self.drop_unit();
                    }
                    None if devs.len() == 1 && declined.as_ref() != Some(&devs[0].0) => self.connect(devs[0].0.clone()),
                    _ => {}
                }
                self.check_sg_driver(connected.is_none() && devs.is_empty());
                let multiple = connected.is_none() && devs.len() > 1;
                if self.shared.lock().unwrap().multiple != multiple {
                    self.with(|s| s.multiple = multiple);
                }
                let midi = match (&connected, devs.is_empty()) {
                    (None, true) => crate::midi::find_units().into_iter().next().map(|u| u.name),
                    _ => None,
                };
                let was = self.shared.lock().unwrap().midi.clone();
                if was != midi {
                    // in the log too: a unit showing up running is how a restart is seen to have worked
                    match (&was, &midi) {
                        (None, Some(n)) => self.log(format!("{n} found on USB MIDI: running")),
                        (Some(n), None) => self.log(format!("{n} left USB MIDI")),
                        _ => {}
                    }
                    self.with(|s| s.midi = midi);
                }
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
    }

    /// Linux: says once, while it lasts, that a unit is in download mode but
    /// can't be seen for want of the SCSI generic driver; the command line
    /// says it too.
    fn check_sg_driver(&self, none_found: bool) {
        let missing = none_found && crate::scsi::needs_sg_driver();
        if self.sg_missing.swap(missing, Ordering::SeqCst) == missing {
            return;
        }
        let text = format!("Refused: {}", crate::scsi::SG_DRIVER);
        if missing {
            self.log(text.clone());
            self.with(|s| s.alert = Some((AlertKind::Refused, text)));
        } else {
            self.with(|s| {
                if s.alert.as_ref().is_some_and(|(_, t)| *t == text) {
                    s.alert = None;
                }
            });
        }
    }


    /// After a write: the unit leaves download mode by the restart (the
    /// command line's `--restart`; the firmware is not running yet, so MIDI
    /// can't do it) and starts what was written. Waits up to 60 s for it on
    /// USB MIDI and says whether it started.
    fn restart_unit(&self, total: usize) {
        let written = format!("{total} sectors written, read back and verified.");
        // the MIDI ports before, as the scan sees them: a port that shows up after the restart is the unit,
        // whatever its firmware calls it
        let before: Vec<String> = self.watched_ports(true).unwrap_or_default().into_iter().map(|(n, _)| n).collect();
        let sent = {
            let mut g = self.unit.lock().unwrap();
            g.as_mut().map(|u| (u.dev.path().to_string(), u.dev.restart()))
        };
        let path = match sent {
            Some((path, Ok(_))) => path,
            Some((_, Err(e))) => {
                self.log(format!("Could not restart the unit ({e}): switch it off and on"));
                self.with(|s| { s.watching_restart = false; s.midi_ports = None; });
                return;
            }
            None => {
                self.with(|s| { s.watching_restart = false; s.midi_ports = None; });
                return;
            }
        };
        self.log("Restarting the unit: it leaves download mode and starts what was written".into());
        self.drop_unit();
        let say = |kind: AlertKind, text: String| self.with(|s| s.alert = Some((kind, text)));
        say(AlertKind::Done, format!("{written} Restarting the FM-1…"));
        let gone = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(250));
            !find_devices().iter().any(|(p, _)| *p == path)
        });
        if !gone {
            self.with(|s| { s.watching_restart = false; s.midi_ports = None; });
            self.log("The unit did not leave download mode".into());
            say(AlertKind::Done, format!("{written} The unit did not restart: switch it off and on to start it."));
            return;
        }
        let running = (0..120).find_map(|_| {
            std::thread::sleep(Duration::from_millis(500));
            if !find_devices().is_empty() {
                return Some(Err(()));
            }
            let ports = self.shared.lock().unwrap().midi_ports.clone().unwrap_or_default();
            ports.iter().find(|(_, unit)| *unit).or_else(|| ports.iter().find(|(n, _)| !before.contains(n)))
                .map(|(n, _)| Ok(n.clone()))
        });
        self.with(|s| { s.watching_restart = false; s.midi_ports = None; });
        match running {
            Some(Ok(name)) => {
                self.log(format!("Running: the {name} is on USB MIDI"));
                say(AlertKind::Done, format!("{written} The FM-1 restarted and is running."));
                return;
            }
            Some(Err(())) => {
                self.log("The unit came back in download mode: what was written did not start".into());
                say(AlertKind::Error, format!("{written} But the unit came back in download mode: what was written \
                    did not start. Its backup is saved: restore it, or install another firmware."));
                return;
            }
            None => {}
        }
        self.log("Restarted; not on USB MIDI within 60 s".into());
        // written and verified, restarted, only not seen: what was written comes first (Figma: scenario 12)
        let what = match self.shared.lock().unwrap().mode {
            Mode::Restore => "The backup was restored",
            Mode::Bootloader => "m-bootloader was installed",
            _ => "The firmware was installed",
        };
        say(AlertKind::Done, format!("{what} and verified ({total} sectors), and the FM-1 restarted. M-Flasher couldn't \
            confirm it's running: the FM-1 didn't show up on USB within 60 seconds. Look at the FM-1 display: if the new \
            firmware shows up, it's ready. If not, switch the FM-1 off and on."));
    }

    /// The MIDI ports the scan reports while a restart is watched (`start`: watching begins, and this waits
    /// for the scan's first report, up to 3 s).
    fn watched_ports(&self, start: bool) -> Option<Vec<(String, bool)>> {
        if start {
            self.with(|s| { s.watching_restart = true; s.midi_ports = None; });
        }
        (0..30).find_map(|_| {
            let p = self.shared.lock().unwrap().midi_ports.clone();
            if p.is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            p
        })
    }

    fn drop_unit(&self) {
        *self.unit.lock().unwrap() = None;
        *self.plan.lock().unwrap() = None;
        self.with(|s| {
            s.device = None;
            s.phase = Phase::Waiting;
            s.plan = None;
            s.marks.clear();
            s.map = vec![Region::Free; 256];
            s.stage = [Stage::Idle; 4];
            s.pct = 0.0;
            s.backup_saved = None;
            s.suggest_boot = None;
        });
    }

    /// Connects; a unit that fails to three times in a row is left alone until
    /// it is reconnected, rather than tried (its loader started, its flash
    /// read) every scan for as long as it stays plugged in.
    fn connect(&self, path: String) {
        self.work("connecting", move |c| {
            let r = c.connect_now(&path).and_then(|()| {
                // INSTALL was waiting for download mode: on with it
                match c.with(|s| { s.how_to = None; s.install_after.take() }) {
                    Some(id) => c.library_plan(&id),
                    None => Ok(()),
                }
            });
            let mut f = c.failures.lock().unwrap();
            match &r {
                Ok(()) => *f = (String::new(), 0),
                Err(_) => {
                    *f = if f.0 == path { (path.clone(), f.1 + 1) } else { (path.clone(), 1) };
                    if f.1 >= CONNECT_TRIES && c.declined.lock().unwrap().as_ref() != Some(&path) {
                        *c.declined.lock().unwrap() = Some(path.clone());
                        c.log(format!("Connecting failed {CONNECT_TRIES} times: not trying again until the unit is reconnected"));
                    }
                }
            }
            r
        });
    }

    fn connect_now(&self, path: &str) -> Result<(), Error> {
        let c = self;
        let path = path.to_string();
        c.with(|s| {
            s.phase = Phase::Connecting;
            s.stage = [Stage::Active, Stage::Idle, Stage::Idle, Stage::Idle];
            s.pct = 0.0;
            s.marks.clear();
        });
        c.log(format!("Unit found in download mode: {path}"));
        let mut log = |m: String| c.log(m);
        let dev = Device::open(&path).inspect_err(|e| {
            // Windows: any failure may have come after the system's dialog
            // (the helper not starting), which every scan would show again
            if cfg!(windows) || matches!(e, ScsiError::NeedsAdmin(_)) {
                *c.declined.lock().unwrap() = Some(path.clone());
            }
        })?;
        // an earlier failure to connect (rights refused, say) is over now
        c.with(|s| {
            if s.alert.as_ref().is_some_and(|(_, t)| t.starts_with("USB error during connecting")) {
                s.alert = None;
            }
        });
        let conn = Connection::start(&path, dev, &mut log)?;
        let backup_dir = c.shared.lock().unwrap().backup_dir.clone();
        let unit = Session::open(conn, backup_dir, &mut log, &mut |d, t| c.with(|s| s.pct = d as f32 / t as f32));
        let unit = match unit {
            Ok(u) => u,
            Err(e) => {
                c.with(|s| {
                    s.phase = Phase::Waiting;
                    s.stage[0] = Stage::Idle;
                });
                return Err(e);
            }
        };
        let mut unit = unit;
        let view = device_view(&unit);
        c.log(format!("{} · {} · bootloader {} · flash read and checked", view.model, view.firmware, view.bootloader));
        c.log(format!("Loader: {}{}", unit.dev.loader(),
            unit.unit_id.as_ref().map(|u| format!(" · unit ID {u}")).unwrap_or_default()));
        let (n, last) = unit.dev.retries();
        if n > 0 {
            c.log(format!("{n} USB transfers had to be sent again (last: {last}): check the cable and port"));
        }
        if !view.certain {
            c.log("The model is not certain: check it under DEVICE INFO before writing".into());
        }
        let map = sector_map(&unit.flash, unit.dev.chip_key(), unit.board());
        remember_unit(&unit, &view);
        unit.show_on_unit();
        *c.unit.lock().unwrap() = Some(unit);
        c.with(|s| {
            s.device = Some(view);
            s.map = map;
            s.phase = Phase::Ready;
            s.stage[0] = Stage::Done;
            s.pct = 0.0;
            s.suggest_boot = None;
        });
        c.replan_now();
        Ok(())
    }

    // ---- planning ----

    fn replan_now(&self) {
        let (mode, files, keep_boot, src) = {
            let s = self.shared.lock().unwrap();
            // (choices change under this lock)
            self.planned.store(self.choices.load(Ordering::SeqCst), Ordering::SeqCst);
            (s.mode, s.files.clone(), s.keep_boot, s.boot_source)
        };
        let guard = self.unit.lock().unwrap();
        let Some(unit) = guard.as_ref() else { return };
        let boot_first = if mode == Mode::Firmware { unit.bootloader_suggestion() } else { None };
        let need = |c: &Controller, what: &str| {
            *c.plan.lock().unwrap() = None;
            c.with(|s| {
                s.marks.clear();
                s.plan = Some(PlanView { kind: PlanKind::Need, title: what.into(), count: 0, ranges: String::new(),
                                         short: String::new(), notes: vec![], warnings: vec![], ready: false });
            });
        };
        let read = |k: &str| -> Option<Result<Vec<u8>, Error>> {
            files.get(k).map(|p| std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {}: {e}", p.display()))))
        };
        let result = match mode {
            Mode::Backup => {
                *self.plan.lock().unwrap() = None;
                let dir = self.shared.lock().unwrap().backup_dir.display().to_string();
                self.with(|s| {
                    s.marks.clear();
                    s.plan = Some(PlanView { kind: PlanKind::Backup, title: "Save a full backup".into(), count: 0,
                        ranges: "0x00000-0xFFFFF".into(), short: String::new(),
                        notes: vec![format!("Reads all 256 sectors (1 MiB) into {dir}; writes nothing to the unit.")],
                        warnings: vec![], ready: true });
                });
                return;
            }
            Mode::Restore => match read("restore") {
                None => return need(self, "Choose a backup file"),
                Some(d) => {
                    let id = files.get("restore").and_then(|p| crate::ops::backup_unit_id(p));
                    d.and_then(|d| {
                        crate::ops::check_backup_file(&files["restore"], &d)?;
                        unit.plan_restore(&d, id.as_deref(), keep_boot)
                    })
                }
            },
            Mode::Firmware => match read("firmware") {
                None => return need(self, "Choose a firmware package (.fwsc)"),
                Some(d) => d.and_then(|d| unit.plan_firmware(&d)),
            },
            Mode::Bootloader => match src {
                BootSource::Open => unit.plan_bootloader(None, None),
                BootSource::Stock => match read("stock") {
                    None => return need(self, "Choose an official package (.fwsc) for its stock bootloader"),
                    Some(d) => d.and_then(|d| unit.plan_bootloader(None, Some(&d))),
                },
                BootSource::File => match read("bootfile") {
                    None => return need(self, "Choose a bootloader file (uboot.boot)"),
                    Some(d) => d.and_then(|d| unit.plan_bootloader(Some(&d), None)),
                },
            },
        };
        drop(guard);
        match result {
            Ok(plan) => {
                let runs = plan.runs();
                let ranges = runs.iter().map(|(a, b)| format!("0x{a:05X}-0x{:05X}", b - 1)).collect::<Vec<_>>().join(", ");
                let short = match (runs.first(), runs.last()) {
                    (Some(f), Some(l)) => format!("0x{:05X}-0x{:05X}{}", f.0, l.1 - 1,
                        if runs.len() > 1 { format!(" IN {} RUNS", runs.len()) } else { String::new() }),
                    _ => "–".into(),
                };
                let view = PlanView { kind: PlanKind::Write, title: plan.title.clone(), count: plan.sectors.len(),
                    ranges, short, notes: plan.notes.clone(), warnings: plan.warnings.clone(), ready: !plan.sectors.is_empty() };
                let marks = plan.sectors.iter().map(|a| (a / SECTOR, Mark::Change)).collect();
                if !plan.sectors.is_empty() {
                    self.log(format!("Plan: {} · {} sectors · {}", plan.title, plan.sectors.len(), view.short));
                }
                *self.plan.lock().unwrap() = Some(plan);
                self.with(|s| {
                    s.marks = marks;
                    s.plan = Some(view);
                    s.boot_first = boot_first;
                });
            }
            Err(e) => {
                *self.plan.lock().unwrap() = None;
                let text = e.to_string();
                self.log(format!("Refused: {text}"));
                self.with(|s| {
                    s.marks.clear();
                    s.plan = Some(PlanView { kind: PlanKind::Refused, title: text.clone(), count: 0, ranges: String::new(),
                                             short: String::new(), notes: vec![], warnings: vec![], ready: false });
                    s.alert = Some((AlertKind::Refused, text));
                });
            }
        }
    }

    fn replan(&self) {
        if self.unit.lock().map(|u| u.is_some()).unwrap_or(false) {
            self.work("planning", |c| {
                c.with(|s| if s.phase == Phase::Done { s.phase = Phase::Ready });
                c.replan_now();
                Ok(())
            });
        }
    }

    // ---- the window's actions ----

    pub fn select_mode(&self, m: Mode) {
        if !self.choose(|s| { let ok = !s.busy; if ok { s.mode = m; } ok }) {
            return;
        }
        self.replan();
    }

    /// Two steps (REQ-BOOT-01): m-bootloader first, for review as any write;
    /// once it is written the firmware, still chosen, is planned and reviewed.
    pub fn bootloader_then_firmware(&self) {
        if self.choose(Shared::begin_two_step) {
            self.replan();
        }
    }

    /// The two steps given up before step 1 was written: back to the firmware.
    pub fn cancel_two_step(&self) {
        if self.choose(Shared::drop_two_step) {
            self.replan();
        }
    }

    /// The firmware's review was opened after step 1.
    pub fn clear_review_next(&self) {
        self.with(|s| s.review_next = false);
    }

    /// The answer to the m-bootloader recommendation: yes chooses installing
    /// it, to be reviewed as any write. Whether it was taken: a yes while the
    /// connection is still being planned leaves the question up, to be answered again.
    pub fn answer_boot_suggestion(&self, install: bool) -> bool {
        let taken = self.choose(|s| {
            if install && s.busy {
                return false;
            }
            s.suggest_boot = None;
            if install {
                s.mode = Mode::Bootloader;
                s.boot_source = BootSource::Open;
            }
            true
        });
        if taken && install {
            self.replan();
        }
        taken
    }

    pub fn set_keep_boot(&self, v: bool) {
        self.choose(|s| s.keep_boot = v);
        self.replan();
    }


    pub fn set_model(&self, key: &str) {
        if let Some(b) = BOARDS.iter().copied().find(|b| b.key == key) {
            if let Some(u) = self.unit.lock().unwrap().as_mut() {
                u.board_override = Some(b);
                u.show_on_unit();
                let view = device_view(u);
                let map = sector_map(&u.flash, u.dev.chip_key(), u.board());
                self.choose(|s| {
                    s.device = Some(view);
                    s.map = map;
                });
            }
            self.log(format!("Model set to {}", b.name));
            self.replan();
        }
    }

    /// Opens a file dialog in a thread; the choice is planned when it returns.
    pub fn choose_file(&self, kind: FileKind) {
        let c = self.clone();
        // counted with the lock a write is admitted under, until the choice is in
        let dir = self.with(|s| {
            self.picking.fetch_add(1, Ordering::SeqCst);
            s.backup_dir.clone()
        });
        std::thread::spawn(move || {
            let d = rfd::FileDialog::new();
            let d = match kind {
                FileKind::Restore => d.set_title("Choose a backup").add_filter("Flash backups", &["bin"]).set_directory(&dir),
                FileKind::Firmware => d.set_title("Choose a firmware package").add_filter("Firmware packages", &["fwsc"]),
                FileKind::Stock => d.set_title("Choose an official package (its stock bootloader is used)").add_filter("Firmware packages", &["fwsc"]),
                FileKind::BootFile => d.set_title("Choose a bootloader file").add_filter("Bootloaders", &["boot"]),
            };
            let picked = d.add_filter("All files", &["*"]).pick_file();
            if let Some(p) = &picked {
                c.log(format!("{} chosen", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
            }
            let chosen = picked.is_some();
            c.with(|s| {
                if let Some(p) = picked {
                    c.choices.fetch_add(1, Ordering::SeqCst);
                    s.files.insert(file_key(kind), p);
                    if kind == FileKind::Firmware {
                        s.library_choice = None;
                    }
                }
                c.picking.fetch_sub(1, Ordering::SeqCst);
            });
            if chosen {
                c.replan();
            }
        });
    }

    /// Reads the firmware library for the Library tab: The Firmware Hub's list
    /// (fetched at most once a day), the official packages imported, and which
    /// packages are on this computer. In a thread; offline, the list on disk.
    pub fn load_library(&self) {
        if !self.with(|s| !s.library_loading && { s.library_loading = true; true }) {
            return;
        }
        let c = self.clone();
        std::thread::spawn(move || {
            use crate::library::{Http, Library, Synced, default_dir};
            let lib = Library::new(default_dir());
            let mut note = match lib.sync(&Http::new(), chrono::Utc::now()) {
                Ok(Synced::Fetched) => "List fetched now".to_string(),
                Ok(Synced::Recent(at)) => format!("List fetched {at}"),
                Err(e) => format!("Offline, the list on disk: {e}"),
            };
            let mut items: Vec<LibItem> = lib.imported().into_iter().map(|(id, version, name)| LibItem {
                id: id.into(), name: name.into(), version: version.into(), byline: "M-VAVE · official, imported".into(),
                kinds: "stock".into(), chip: "any".into(), beta: false, saved: true, elsewhere: false, older: vec![],
                favorite: false,
            }).collect();
            match lib.catalog() {
                Ok(Some(cat)) => {
                    lib.index_saved(&cat);
                    note = format!("{note} · {} firmwares, list updated {}", cat.entries.len(), cat.updated);
                    for e in &cat.entries {
                        if items.iter().any(|i| i.id == e.id) {
                            continue;
                        }
                        items.push(LibItem {
                            id: e.id.clone(), name: e.name.clone(), version: e.version.clone(),
                            byline: format!("{} · {}", if e.author.is_empty() { "?" } else { &e.author }, e.license),
                            kinds: e.kinds.join(" "), chip: e.chip.clone(), beta: e.beta, saved: lib.downloaded(e),
                            elsewhere: e.elsewhere, older: lib.older_versions(e), favorite: false,
                        });
                    }
                }
                Ok(None) => note = format!("{note} · no list on this computer yet"),
                Err(e) => note = format!("{note} · {e}"),
            }
            let favorites = lib.favorites();
            for i in &mut items {
                i.favorite = favorites.contains(&i.id);
            }
            c.with(|s| {
                s.library = items;
                s.library_note = note;
                s.library_loading = false;
            });
        });
    }

    /// Downloads a firmware of the library from its author's release and keeps
    /// it only if its SHA-256 matches the list (GET): no unit needed, for
    /// installing later, offline too.
    pub fn library_get(&self, id: String) {
        if !self.with(|s| !s.library_getting.contains(&id) && { s.library_getting.push(id.clone()); true }) {
            return;
        }
        let c = self.clone();
        std::thread::spawn(move || {
            use crate::library::{Http, Library, default_dir};
            let lib = Library::new(default_dir());
            let result = lib.catalog().and_then(|cat| {
                let cat = cat.ok_or("no firmware list on this computer: RELOAD")?;
                let e = cat.find(&id).ok_or_else(|| format!("{id} is no longer in the firmware list"))?.clone();
                c.log(format!("Library: downloading {} {}", e.name, e.version));
                lib.get(&e, &Http::new()).map(|(path, identity)| (e, path, identity))
            });
            match &result {
                Ok((e, path, identity)) => c.log(format!("Library: {} {} saved ({identity}, SHA-256 matches the list): {}",
                                                       e.name, e.version, path.display())),
                Err(err) => c.log(format!("Refused: {err}")),
            }
            c.with(|s| {
                s.library_getting.retain(|x| x != &id);
                match result {
                    Ok(_) => if let Some(i) = s.library.iter_mut().find(|i| i.id == id) { i.saved = true; i.older.clear(); },
                    Err(err) => s.alert = Some((AlertKind::Refused, format!("Refused: {err}"))),
                }
            });
        });
    }

    /// Stars or unstars a firmware of the library (kept on this computer).
    pub fn library_favorite(&self, id: String, on: bool) {
        use crate::library::{Library, default_dir};
        match Library::new(default_dir()).set_favorite(&id, on) {
            Ok(()) => self.with(|s| if let Some(i) = s.library.iter_mut().find(|i| i.id == id) { i.favorite = on; }),
            Err(e) => self.log(format!("Refused: {e}")),
        }
    }

    /// Deletes every downloaded package of a firmware from this computer (the
    /// window asked first); the unit is not touched.
    pub fn library_delete(&self, id: String) {
        use crate::library::{Library, default_dir};
        match Library::new(default_dir()).delete(&id) {
            Ok(gone) => {
                self.log(format!("Library: deleted {} file(s) of {id}", gone.len()));
                self.with(|s| {
                    if s.files.get("firmware").is_some_and(|f| gone.contains(f)) {
                        s.files.remove("firmware");
                    }
                    if let Some(i) = s.library.iter().position(|i| i.id == id) {
                        if s.library[i].byline.starts_with("M-VAVE · official") {
                            s.library.remove(i);
                        } else {
                            s.library[i].saved = false;
                            s.library[i].older.clear();
                        }
                    }
                });
                self.replan();
            }
            Err(e) => {
                self.log(format!("Refused: {e}"));
                self.with(|s| s.alert = Some((AlertKind::Refused, format!("Refused: {e}"))));
            }
        }
    }

    /// Installs a firmware of the library: downloaded and checked if needed,
    /// then chosen as the firmware to flash and planned, for REVIEW & FLASH. A
    /// unit with M-VAVE's stock bootloader is offered m-bootloader first
    /// (REQ-BOOT-01).
    pub fn library_install(&self, id: String) {
        self.work("the library", move |c| {
            // download mode first: the soft key, or the keys by hand (then the scan carries on)
            if c.unit.lock().unwrap().is_none() {
                match c.reach_download_mode(true)? {
                    Some(path) => c.connect_now(&path)?,
                    None => {
                        c.with(|s| s.install_after = Some(id));
                        return Ok(());
                    }
                }
            }
            c.library_plan(&id)
        });
    }

    /// INSTALL's wait for download mode given up.
    /// CHECK FM1 STATUS: the unit connected is read again; otherwise one in
    /// download mode, or a running one switched by the soft key (or by hand:
    /// `how_to`), is read. Nothing is written.
    pub fn check(&self) {
        self.work("checking the unit", |c| {
            let kept = c.unit.lock().unwrap().as_ref().map(|u| u.dev.path().to_string());
            if let Some(path) = kept {
                c.drop_unit();
                return c.connect_now(&path);
            }
            match c.reach_download_mode(false)? {
                Some(path) => c.connect_now(&path),
                // switched by hand: the scan reads it when it shows up
                None => Ok(()),
            }
        });
    }

    pub fn cancel_install_wait(&self) {
        if self.with(|s| { s.how_to = None; s.install_after.take() }).is_some() {
            self.log("Install cancelled".into());
        }
    }

    pub fn clear_library_review(&self) {
        self.with(|s| s.library_review = false);
    }

    /// The unit in download mode for INSTALL: the one already in it, or the one
    /// on USB MIDI switched by the soft key. None when it has to be switched by
    /// hand: `how_to` then says how, for this unit as last read.
    fn reach_download_mode(&self, install: bool) -> Result<Option<String>, Error> {
        let devs = find_devices();
        match devs.len() {
            0 => {}
            1 => return Ok(Some(devs[0].0.clone())),
            _ => return Err(Error::Refused("more than one unit in download mode: connect only one".into())),
        }
        let units = crate::midi::find_units();
        if units.len() > 1 {
            return Err(Error::Refused("more than one unit on USB MIDI: connect only one".into()));
        }
        let running = units.first().map(|u| u.name.clone());
        if let Some(u) = units.first() {
            match u.soft_key() {
                Ok(()) => {
                    self.log(format!("Sent the {} the download-mode message over USB MIDI; waiting for it", u.name));
                    self.with(|s| s.midi = None);
                    for _ in 0..40 {
                        std::thread::sleep(Duration::from_millis(500));
                        if let Some(d) = find_devices().into_iter().next() {
                            return Ok(Some(d.0));
                        }
                        if crate::scsi::needs_sg_driver() {
                            self.sg_missing.store(true, Ordering::SeqCst);
                            return Err(Error::Refused(crate::scsi::SG_DRIVER.into()));
                        }
                    }
                    self.log(format!("The {} did not come back in download mode within 20 s: its firmware may not \
                                      know the message", u.name));
                }
                Err(e) => self.log(format!("Could not send the {} the download-mode message: {e}", u.name)),
            }
        } else {
            self.log("No unit in download mode or running on USB MIDI".into());
        }
        let (rescue, firmware, model) = last_unit();
        self.with(|s| s.how_to = Some(how_to(&rescue, &firmware, &model, running.as_deref(), install)));
        Ok(None)
    }

    /// Plans installing a firmware of the library on the unit connected; the
    /// window opens its review.
    fn library_plan(&self, id: &str) -> Result<(), Error> {
        let c = self;
        let id = id.to_string();
        {
            use crate::library::{Http, Library, default_dir, unit_chip};
            let lib = Library::new(default_dir());
            let inst = lib.installable(&id, &Http::new()).map_err(Error::Refused)?;
            {
                let g = c.unit.lock().unwrap();
                let u = g.as_ref().ok_or_else(|| Error::Refused("connect a unit in download mode first".into()))?;
                if !inst.fits_chip(unit_chip(u.dev.flash_id())) {
                    return Err(Error::Refused(format!("{} is marked for the FM-1's older flash chip (0x856014) only; \
                                                       this unit has the newer one", inst.name)));
                }
            }
            // m-bootloader first, on a unit with the stock one: REVIEW & FLASH opens the two steps
            c.log(format!("Library: {} {} chosen ({})", inst.name, inst.version, inst.credit));
            c.with(|s| s.library_choice = Some(format!("{} {}", inst.name, inst.version)));
            c.choose(|s| {
                s.files.insert("firmware", inst.path.clone());
                s.mode = Mode::Firmware;
                if let Some(i) = s.library.iter_mut().find(|i| i.id == inst.id) {
                    i.saved = true;
                }
            });
            c.with(|s| if s.phase == Phase::Done { s.phase = Phase::Ready });
            c.replan_now();
            c.with(|s| s.library_review = true);
            Ok(())
        }
    }

    pub fn pick_backup(&self, path: PathBuf) {
        self.choose(|s| {
            s.files.insert("restore", path);
            s.mode = Mode::Restore;
        });
        self.replan();
    }

    pub fn save_backup(&self) {
        self.work("backup", |c| {
            c.with(|s| {
                s.phase = Phase::Working;
                s.stage[1] = Stage::Active;
            });
            let path = {
                let mut g = c.unit.lock().unwrap();
                let u = g.as_mut().ok_or_else(|| Error::Refused("no unit connected".into()))?;
                u.save_backup(None)?
            };
            c.log(format!("Backup saved: {}", path.display()));
            c.refresh_backups();
            c.with(|s| {
                s.phase = Phase::Done;
                s.stage[1] = Stage::Done;
                s.backup_saved = path.file_name().map(|n| n.to_string_lossy().into_owned());
                s.alert = Some((AlertKind::Done, format!("Backup saved:\n{}", path.display())));
            });
            Ok(())
        });
    }

    /// Write the reviewed plan.
    pub fn confirm(&self) {
        // not a plan made before the latest choice: that one is planned first,
        // for the user to review
        // (Checked as the write is admitted: choices change with the same lock
        // held, so none slips in between, and once it runs nothing is planned.)
        let (mut stale, mut picking) = (false, false);
        let fresh = |_: &Shared| {
            // a file dialog still open may change the choice: it is closed first
            picking = self.picking.load(Ordering::SeqCst) != 0;
            stale = self.choices.load(Ordering::SeqCst) != self.planned.load(Ordering::SeqCst);
            !picking && !stale
        };
        self.work_if("writing", fresh, move |c| {
            let Some(plan) = c.plan.lock().unwrap().clone().filter(|p| !p.sectors.is_empty()) else { return Ok(()) };
            let total = plan.sectors.len();
            c.with(|s| {
                s.phase = Phase::Working;
                s.alert = None;
                s.stage = [Stage::Done, if s.backup_saved.is_some() { Stage::Done } else { Stage::Idle }, Stage::Idle, Stage::Idle];
                s.pct = 0.0;
                s.cur = None;
            });
            let mut run = 0usize;
            let mut event = |e: Event| {
                c.with(|s| {
                    match e {
                        Event::Backup => s.stage[1] = Stage::Active,
                        Event::Erase(a) => {
                            s.stage[1] = Stage::Done;
                            s.stage[2] = Stage::Active;
                            s.stage[3] = Stage::Idle;
                            run = a / SECTOR;
                            s.marks.insert(run, Mark::Now);
                            s.cur = Some(a);
                        }
                        Event::Write(a) => {
                            for sec in run..a / SECTOR {
                                if matches!(s.marks.get(&sec), Some(Mark::Change | Mark::Now)) {
                                    s.marks.insert(sec, Mark::Written);
                                }
                            }
                            if s.marks.get(&(a / SECTOR)) == Some(&Mark::Change) {
                                s.marks.insert(a / SECTOR, Mark::Now);
                            }
                            s.cur = Some(a);
                        }
                        Event::Verify(a) => {
                            s.stage[3] = Stage::Active;
                            for sec in run..a / SECTOR {
                                s.marks.insert(sec, Mark::Verified);
                            }
                            s.cur = Some(a);
                        }
                    }
                    let n = if matches!(e, Event::Verify(_)) {
                        s.marks.values().filter(|m| **m == Mark::Verified).count()
                    } else {
                        s.marks.values().filter(|m| matches!(m, Mark::Written | Mark::Verified)).count()
                    };
                    s.pct = n as f32 / total as f32;
                });
            };
            let mut log_lines = vec![];
            let have_backup;
            let result = {
                let mut g = c.unit.lock().unwrap();
                let u = g.as_mut().ok_or_else(|| Error::Refused("no unit connected".into()))?;
                let r = u.apply(&plan, &mut event, &mut |m| log_lines.push(m));
                have_backup = u.backup_path.is_some();
                let saved = u.backup_path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                let view = device_view(u);
                let map = sector_map(&u.flash, u.dev.chip_key(), u.board());
                c.with(|s| {
                    s.backup_saved = saved;
                    s.device = Some(view);
                    s.map = map;
                });
                r
            };
            for m in log_lines {
                c.log(m);
            }
            c.log_retries();
            if result.is_err() && !have_backup {
                c.log("Nothing was written: the backup that comes first could not be saved.".into());
            } else if result.is_err() {
                c.log("Nothing is lost: the backup is saved. Choose the same operation again to finish it; only what is \
                       still missing is written.".into());
            }
            c.refresh_backups();
            result?;
            c.with(|s| {
                s.phase = Phase::Done;
                s.stage = [Stage::Done; 4];
                s.pct = 1.0;
                s.cur = None;
                s.marks = plan.sectors.iter().map(|a| (a / SECTOR, Mark::Verified)).collect();
                if let Some(p) = s.plan.as_mut() {
                    p.ready = false;
                }
                s.alert = Some((AlertKind::Done, format!("{total} sectors written, read back and verified. Unplug the unit \
                    and switch it off and on to start it, or choose another operation.")));
            });
            *c.plan.lock().unwrap() = None;
            // step 1 of 2 written (m-bootloader): the firmware, still chosen, is step 2
            if c.choose(|s| s.end_step_one(total)) {
                c.log("Step 1 of 2 done: m-bootloader written and verified. Step 2: the firmware".into());
                c.replan_now();
            } else {
                // done: the unit leaves download mode and starts what was written
                c.restart_unit(total);
            }
            Ok(())
        });
        if picking {
            self.log("A file is still being chosen: close its window first, nothing written".into());
        } else if stale {
            self.log("The choices changed after this plan was made: planning again, nothing written".into());
            self.replan();
        }
    }

    /// Log how many USB transfers were sent again (they hint at a poor cable or port).
    fn log_retries(&self) {
        let r = self.unit.lock().unwrap().as_ref().map(|u| u.dev.retries());
        if let Some((n, last)) = r {
            if n > 0 {
                self.log(format!("{n} USB transfers had to be sent again so far (last: {last}): check the cable and port"));
            }
        }
    }

    pub fn dismiss(&self) {
        let done = self.with(|s| {
            s.alert = None;
            let done = s.phase == Phase::Done;
            if done {
                s.stage[2] = Stage::Idle;
                s.stage[3] = Stage::Idle;
                s.pct = 0.0;
            }
            done
        });
        if done {
            self.replan();
        }
    }

    pub fn clear_log(&self) {
        self.with(|s| s.log.clear());
    }

    pub fn open_backups(&self) {
        let dir = self.shared.lock().unwrap().backup_dir.clone();
        let _ = crate::ops::create_dirs(&dir);
        shell_open(dir.as_os_str());
    }

    /// A web page, in the user's browser.
    pub fn open_link(&self, url: &str) {
        shell_open(std::ffi::OsStr::new(url));
    }

    fn refresh_backups(&self) {
        let dir = self.shared.lock().unwrap().backup_dir.clone();
        let list = recent_backups(&dir, 6);
        self.with(|s| s.backups = list);
    }
}

/// A folder or a web page, in the file manager or the browser. Through the
/// shell, not by running "explorer": Windows would look for that next to this
/// executable first, and start what it finds as administrator. On its own
/// thread, as the shell can take a while, and it wants COM there.
#[cfg(windows)]
fn shell_open(target: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    const SW_SHOWNORMAL: i32 = 1;
    let target: Vec<u16> = target.encode_wide().chain(std::iter::once(0)).collect();
    std::thread::spawn(move || {
        let open: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: both are valid NUL-terminated strings; the rest may be null.
        unsafe {
            let com = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
            ShellExecuteW(std::ptr::null_mut(), open.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
            if com >= 0 {
                CoUninitialize();
            }
        }
    });
}

#[cfg(not(windows))]
fn shell_open(target: &std::ffi::OsStr) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut cmd = match std::env::var("SUDO_USER") {
        // run with sudo: the file manager or browser opens as the user, not as root
        Ok(user) => {
            let mut c = std::process::Command::new("sudo");
            c.args(["-u", &user, opener]);
            c
        }
        _ => std::process::Command::new(opener),
    };
    let _ = cmd.arg(target).spawn();
}

fn device_view(u: &Unit) -> DeviceView {
    let a = &u.analysis;
    let model = u.board_override.map(|b| b.name.to_string()).unwrap_or_else(|| a.model_text());
    DeviceView {
        model,
        certain: a.certain || u.board_override.is_some(),
        firmware: a.firmware_text.clone(),
        app: a.app_status.text().to_string(),
        app_ok: a.app_status == crate::formats::AppStatus::Ok,
        // the model is shown next to it: no need for the build's board
        bootloader: match a.bootloader {
            crate::boards::BootKind::Open => a.bootloader_text.split(" (").next().unwrap_or_default().to_string(),
            _ => a.bootloader_text.clone(),
        },
        boot_open: a.bootloader == crate::boards::BootKind::Open,
        flash: flash_text(u.dev.flash_id()),
        flash_label: match crate::boards::flash_name(u.dev.flash_id()) {
            Some(n) => format!("{n} · {}", flash_text(u.dev.flash_id())),
            None => flash_text(u.dev.flash_id()),
        },
        chip_key: format!("0x{:04X}", u.dev.chip_key()),
        chip_key_ok: u.dev.chip_key() == CHIP_KEY,
        evidence: a.evidence.iter().map(|e| e.2.clone()).collect(),
    }
}

fn recent_backups(dir: &Path, n: usize) -> Vec<BackupInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "bin"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().take(n).map(|(t, p)| {
        let json = std::fs::read_to_string(p.with_extension("json")).unwrap_or_default();
        let firmware = json.split("\"firmware\": \"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string();
        let time: chrono::DateTime<chrono::Local> = t.into();
        BackupInfo { name: p.file_name().unwrap().to_string_lossy().into_owned(), path: p,
                     time: time.format("%Y-%m-%d %H:%M").to_string(), firmware }
    }).collect()
}

/// Where the last unit read is remembered: the keys that switch it by hand.
fn unit_memo() -> PathBuf {
    crate::library::default_dir().join("last-unit.txt")
}

/// Remembers what INSTALL needs to say how to switch this unit by hand.
fn remember_unit(u: &Unit, view: &DeviceView) {
    let rescue = match u.analysis.bootloader {
        crate::boards::BootKind::Open => "yes",
        crate::boards::BootKind::Stock => "no",
        _ => "",
    };
    let memo = unit_memo();
    let _ = std::fs::create_dir_all(memo.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(memo, format!("rescue={rescue}\nfirmware={}\nmodel={}\n", view.firmware, view.model));
}

/// The last unit read: rescue ("yes", "no", "" unknown), firmware, model.
fn last_unit() -> (String, String, String) {
    let text = std::fs::read_to_string(unit_memo()).unwrap_or_default();
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
        .unwrap_or_default().to_string();
    (get("rescue"), get("firmware"), get("model"))
}

/// How to put the unit in download mode by hand, from what was last read on
/// it (as the Android app says it): m-bootloader's key while switching on,
/// OCT- and OCT+ on the firmwares that know them; M-VAVE's own firmware on the
/// stock bootloader only by the soft key. `running`: its name on USB MIDI.
/// The unit as the user knows it: "M-Vave FM-1" or "M-Vave SMK-37", whatever
/// name its firmware gives its MIDI port.
pub(crate) fn unit_name(model: &str, midi: &str) -> &'static str {
    if model.contains("SMK") || midi.to_uppercase().contains("SMK") { "M-Vave SMK-37" } else { "M-Vave FM-1" }
}

pub(crate) fn how_to(rescue: &str, firmware: &str, model: &str, running: Option<&str>, install: bool) -> String {
    let rec = if model.contains("SMK") { "SEQ REC" } else { "REC" };
    let u = if model.contains("SMK") { "SMK-37" } else { "FM-1" };
    let fw = format!("{firmware} {}", running.unwrap_or_default()).to_lowercase();
    let oct = ["felucca", "caravel", "sloop", "snoop"].iter().any(|n| fw.contains(n));
    let why = match running {
        // the unit by its model, not by the name its firmware gives the MIDI port ("Caravel"): that is metadata
        Some(n) => format!("The {} did not switch automatically to Download Mode. Switch it manually following these steps:",
                           unit_name(model, n)),
        None => format!("M-Flasher can't see your M-Vave {u} running. Check it is connected and switched on, or switch it \
                         manually following these steps:"),
    };
    let keys = match rescue {
        "yes" => format!("1 · Switch it off.\n2 · Hold {rec} and switch {u} on.\n3 · When M-Flasher finds your {u}, release the \
                          {rec} button.{}",
                         if oct { format!("\nOr, if your {u} is running, hold OCT- and OCT+ buttons together for 5 seconds.") } else { String::new() }),
        _ if oct => format!("If your {u} is running, hold OCT- and OCT+ buttons together for 5 seconds."),
        "no" => format!("It has M-VAVE's original bootloader: its buttons can't do it, only the message from M-Flasher. Switch \
                         it on, connect it with a data cable (some cables only charge) and press {} again.",
                        if install { "INSTALL" } else { "CHECK FM1 STATUS" }),
        _ => format!("· With m-bootloader: hold {rec} while switching {u} on.\n\
                      · With Felucca, SLOOP or Snoop37 running: hold OCT- and OCT+ buttons together for 5 seconds.\n\
                      · With M-VAVE's own firmware: switch it on, connect it and press {} again: M-Flasher switches it.",
                     if install { "INSTALL" } else { "CHECK FM1 STATUS" }),
    };
    format!("{why}\n{keys}\nM-Flasher needs Download Mode to check your {u} properly.")
}

/// The operation log kept on disk too, next to the backups' folder
/// (Documents/M-Flasher/m-flasher.log): the window shows only its last lines,
/// and they go with it. Past 1 MiB the file starts again, the last one kept as
/// m-flasher.log.1.
pub(crate) fn log_to_file(backup_dir: &Path, text: &str) {
    use std::io::Write;
    let path = backup_dir.parent().unwrap_or(backup_dir).join("m-flasher.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(backup_dir));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {text}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}
