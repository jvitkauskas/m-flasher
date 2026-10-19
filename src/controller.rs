//! The window's logic, apart from the drawing: finds the unit, connects, plans
//! the selected operation and writes it with progress per sector, in
//! background threads. Everything the window shows is in `Shared`, which the
//! window copies each frame; the threads change it and ask for a repaint.

use crate::boards::{BOARDS, CHIP_KEY};
use crate::cli::default_backup_dir;
use crate::formats::SECTOR;
use crate::jl::{Connection, Flash};
use crate::ops::{Error, Event, Plan, Session, bundled_bootloader};
use crate::scsi::{Device, find_devices};
use crate::sectormap::{Region, sector_map};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    pub flash: String,
    pub chip_key: String,
    pub chip_key_ok: bool,
    pub evidence: Vec<String>,
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
    pub wipe: bool,
    pub with_boot: bool,
    pub boot_source: BootSource,
    /// A unit running normally, found on USB MIDI (its name), while none is in
    /// download mode: the soft key can switch it.
    pub midi: Option<String>,
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
    ctx: egui::Context,
}

use eframe::egui;

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl Controller {
    pub fn new(ctx: egui::Context) -> Controller {
        let open_version = bundled_bootloader(BOARDS[1]).map(|(_, v)| v).unwrap_or_else(|_| "?".into());
        let backup_dir = default_backup_dir();
        let c = Controller {
            shared: Arc::new(Mutex::new(Shared {
                phase: Phase::Waiting,
                busy: false,
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
                open_version: open_version.clone(),
                mode: Mode::Firmware,
                files: HashMap::new(),
                wipe: false,
                with_boot: false,
                boot_source: BootSource::Open,
                midi: None,
            })),
            unit: Arc::new(Mutex::new(None)),
            plan: Arc::new(Mutex::new(None)),
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
        self.with(|s| {
            s.log.push(format!("[{}] {text}", now()));
            let n = s.log.len();
            if n > 200 {
                s.log.drain(..n - 200);
            }
        });
    }

    /// Run f in a thread unless something is running; errors become alerts.
    fn work(&self, name: &'static str, f: impl FnOnce(&Controller) -> Result<(), Error> + Send + 'static) {
        if !self.with(|s| !std::mem::replace(&mut s.busy, true)) {
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
        });
    }

    // ---- connection ----

    fn scan_loop(&self) {
        loop {
            if !self.shared.lock().unwrap().busy {
                let devs = find_devices();
                let connected = self.unit.lock().unwrap().as_ref().map(|u| u.dev.path().to_string());
                match &connected {
                    Some(p) if !devs.iter().any(|d| &d.0 == p) => {
                        self.log("The unit is gone (unplugged or switched off)".into());
                        self.drop_unit();
                    }
                    None if devs.len() == 1 => self.connect(devs[0].0.clone()),
                    _ => {}
                }
                let multiple = connected.is_none() && devs.len() > 1;
                if self.shared.lock().unwrap().multiple != multiple {
                    self.with(|s| s.multiple = multiple);
                }
                let midi = match (&connected, devs.is_empty()) {
                    (None, true) => crate::midi::find_units().into_iter().next().map(|u| u.name),
                    _ => None,
                };
                if self.shared.lock().unwrap().midi != midi {
                    self.with(|s| s.midi = midi);
                }
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
    }

    /// Switch the unit found on USB MIDI to download mode (the soft key); the
    /// scan then finds and connects it.
    pub fn enter_download_mode(&self) {
        self.work("switching to download mode", |c| {
            let units = crate::midi::find_units();
            let unit = match units.len() {
                0 => return Err(Error::Refused("the unit is no longer on USB MIDI".into())),
                1 => &units[0],
                _ => return Err(Error::Refused("more than one unit on USB MIDI: connect only one".into())),
            };
            c.log(format!("Switching the {} to download mode over USB MIDI", unit.name));
            unit.soft_key().map_err(Error::Io)?;
            c.with(|s| s.midi = None);
            Ok(())
        });
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
        });
    }

    fn connect(&self, path: String) {
        self.work("connecting", move |c| {
            c.with(|s| {
                s.phase = Phase::Connecting;
                s.stage = [Stage::Active, Stage::Idle, Stage::Idle, Stage::Idle];
                s.pct = 0.0;
                s.marks.clear();
            });
            c.log(format!("Unit found in download mode: {path}"));
            let mut log = |m: String| c.log(m);
            let conn = Connection::start(&path, Device::open(&path)?, &mut log)?;
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
            let mut unit = unit;
            unit.show_on_unit();
            *c.unit.lock().unwrap() = Some(unit);
            c.with(|s| {
                s.device = Some(view);
                s.map = map;
                s.phase = Phase::Ready;
                s.stage[0] = Stage::Done;
                s.pct = 0.0;
            });
            c.replan_now();
            Ok(())
        });
    }

    // ---- planning ----

    fn replan_now(&self) {
        let (mode, files, wipe, with_boot, src) = {
            let s = self.shared.lock().unwrap();
            (s.mode, s.files.clone(), s.wipe, s.with_boot, s.boot_source)
        };
        let guard = self.unit.lock().unwrap();
        let Some(unit) = guard.as_ref() else { return };
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
                    d.and_then(|d| unit.plan_restore(&d, id.as_deref()))
                }
            },
            Mode::Firmware => match read("firmware") {
                None => return need(self, "Choose a firmware package (.fwsc)"),
                Some(d) => d.and_then(|d| unit.plan_firmware(&d, with_boot, wipe)),
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
        if !self.with(|s| { let ok = !s.busy; if ok { s.mode = m; } ok }) {
            return;
        }
        self.replan();
    }

    pub fn set_wipe(&self, v: bool) {
        self.with(|s| s.wipe = v);
        self.replan();
    }

    pub fn set_with_boot(&self, v: bool) {
        self.with(|s| s.with_boot = v);
        self.replan();
    }

    pub fn set_boot_source(&self, v: BootSource) {
        self.with(|s| s.boot_source = v);
        self.replan();
    }

    pub fn set_model(&self, key: &str) {
        if let Some(b) = BOARDS.iter().copied().find(|b| b.key == key) {
            if let Some(u) = self.unit.lock().unwrap().as_mut() {
                u.board_override = Some(b);
                u.show_on_unit();
                let view = device_view(u);
                let map = sector_map(&u.flash, u.dev.chip_key(), u.board());
                self.with(|s| {
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
        let dir = self.shared.lock().unwrap().backup_dir.clone();
        std::thread::spawn(move || {
            let d = rfd::FileDialog::new();
            let d = match kind {
                FileKind::Restore => d.set_title("Choose a backup").add_filter("Flash backups", &["bin"]).set_directory(&dir),
                FileKind::Firmware => d.set_title("Choose a firmware package").add_filter("Firmware packages", &["fwsc"]),
                FileKind::Stock => d.set_title("Choose an official package (its stock bootloader is used)").add_filter("Firmware packages", &["fwsc"]),
                FileKind::BootFile => d.set_title("Choose a bootloader file").add_filter("Bootloaders", &["boot"]),
            };
            if let Some(p) = d.add_filter("All files", &["*"]).pick_file() {
                c.log(format!("{} chosen", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                c.with(|s| { s.files.insert(file_key(kind), p); });
                c.replan();
            }
        });
    }

    pub fn pick_backup(&self, path: PathBuf) {
        self.with(|s| {
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
        let Some(plan) = self.plan.lock().unwrap().clone() else { return };
        if plan.sectors.is_empty() {
            return;
        }
        self.work("writing", move |c| {
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
            let result = {
                let mut g = c.unit.lock().unwrap();
                let u = g.as_mut().ok_or_else(|| Error::Refused("no unit connected".into()))?;
                let r = u.apply(&plan, &mut event, &mut |m| log_lines.push(m));
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
            if result.is_err() {
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
            Ok(())
        });
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
        let opener = if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let mut cmd = match std::env::var("SUDO_USER") {
            // run with sudo: the file manager opens as the user, not as root
            Ok(user) if !cfg!(windows) => {
                let mut c = std::process::Command::new("sudo");
                c.args(["-u", &user, opener]);
                c
            }
            _ => std::process::Command::new(opener),
        };
        let _ = cmd.arg(&dir).spawn();
    }

    fn refresh_backups(&self) {
        let dir = self.shared.lock().unwrap().backup_dir.clone();
        let list = recent_backups(&dir, 6);
        self.with(|s| s.backups = list);
    }
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
        flash: format!("0x{:06X}", u.dev.flash_id()),
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
