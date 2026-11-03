//! The window: an FM-synth front panel with the flash map, drawn with egui on
//! a fixed 1536 x 1024 board scaled to the window. All state comes from the
//! controller; this module only draws it and turns clicks into actions.

use crate::controller::{AlertKind, BootSource, Controller, FileKind, Mark, Mode, Phase, PlanKind, Shared, Stage};
use crate::sectormap::{Region, region_ranges};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Mesh, Pos2, Rect, Sense,
    Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, TextureWrapMode, pos2, vec2,
};
use std::cell::RefCell;

const W: f32 = 1536.0;
const H: f32 = 1024.0;

const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}
const BG: Color32 = hex(0x28221f);
const BG2: Color32 = hex(0x221d1a);
const INK: Color32 = hex(0xe9e4dc);
const DIM: Color32 = hex(0x8d837b);
const LAV: Color32 = hex(0xb6a2e6);
const LAVLINE: Color32 = hex(0x5d5172);
const MINT: Color32 = hex(0x7be8bb);
const KEY_TOP: Color32 = hex(0x23706f);
const KEY_BOT: Color32 = hex(0x174b4d);
const KEYLINE: Color32 = hex(0x2f8888);
const KEYINK: Color32 = hex(0xd6efe8);
const LCD1: Color32 = hex(0xb8cf66);
const LCD2: Color32 = hex(0xa1b94f);
const LCDINK: Color32 = hex(0x1d2710);
const NOW: Color32 = hex(0xff4a2a);
const WARN: Color32 = hex(0xf2b44c);
const BAD: Color32 = hex(0xff6a5a);

fn region_color(r: Region) -> Color32 {
    match r {
        Region::Boot => hex(0xa2405a),
        Region::Firmware => hex(0x5ed4bf),
        Region::Settings => hex(0xdcc395),
        Region::Pairings => hex(0x6f97cf),
        Region::Calibration => hex(0xf0e8d2),
        Region::Presets => hex(0xbd6e2a),
        Region::Sequences => hex(0x9e85c5),
        Region::Sounds => hex(0xc6717a),
        Region::Mac => hex(0x8f8b86),
        Region::Free => hex(0x3a3532),
    }
}

#[derive(PartialEq)]
enum Dialog {
    None,
    Review,
    About,
    Model,
}

pub struct App {
    ctl: Controller,
    dialog: Dialog,
    noise: Option<TextureHandle>,
    /// When an inactive key was last pressed: the LCD's bottom line, which
    /// says what is needed, flashes for a moment.
    nudged: f64,
    fonts: crate::fonts::Fonts,
}

/// How long the LCD's bottom line flashes after an inactive key is pressed (s).
const NUDGE: f64 = 1.2;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let fonts = crate::fonts::Fonts::default();
        fonts.install(&cc.egui_ctx);
        // MFLASHER_DIALOG=about opens the About dialog at start (for screenshots)
        let dialog = if std::env::var("MFLASHER_DIALOG").as_deref() == Ok("about") { Dialog::About } else { Dialog::None };
        App { ctl: Controller::new(cc.egui_ctx.clone()), dialog, noise: None, nudged: f64::NEG_INFINITY, fonts }
    }
}

/// The LCD's bottom line for the current state.
fn prompt(s: &Shared) -> String {
    let n_done = s.marks.values().filter(|m| matches!(m, Mark::Written | Mark::Verified)).count();
    match (&s.device, s.phase) {
        (None, Phase::Connecting) => "DO NOT UNPLUG OR SWITCH OFF".into(),
        (None, _) if s.multiple => "CONNECT ONLY ONE UNIT".into(),
        (None, _) if s.midi.is_some() => format!("{} RUNNING · PRESS DOWNLOAD MODE", s.midi.clone().unwrap_or_default().to_uppercase()),
        (None, _) => "HOLD SEQ REC (SMK-37) OR REC (FM-1) WHILE SWITCHING ON".into(),
        (Some(_), Phase::Working) if s.mode == Mode::Backup => "SAVING FULL BACKUP · 1 MiB".into(),
        (Some(_), Phase::Working) if s.stage[2] == Stage::Idle => "SAVING A BACKUP FIRST · DO NOT SWITCH OFF".into(),
        (Some(_), Phase::Working) => {
            let total = s.plan.as_ref().map(|p| p.count).unwrap_or(0);
            let verb = if s.stage[3] == Stage::Active { "VERIFYING" } else { "WRITING" };
            format!("{verb} 0x{:05X} · {n_done} / {total} · DO NOT SWITCH OFF", s.cur.unwrap_or(0))
        }
        (Some(_), Phase::Done) if s.mode == Mode::Backup => format!("BACKUP SAVED · {}", s.backup_saved.clone().unwrap_or_default()),
        (Some(_), Phase::Done) => format!("DONE · {} SECTORS VERIFIED · SWITCH OFF AND ON", s.plan.as_ref().map(|p| p.count).unwrap_or(0)),
        (Some(_), _) => match &s.plan {
            None => "CHOOSE AN OPERATION BELOW".into(),
            Some(p) => match p.kind {
                PlanKind::Refused => format!("REFUSED · {}", p.title.to_uppercase()),
                PlanKind::Need => p.title.to_uppercase(),
                PlanKind::Backup => "SAVE FULL BACKUP · 1 MiB · WRITES NOTHING".into(),
                PlanKind::Write if p.count == 0 => "NOTHING TO WRITE · ALREADY ON THE UNIT".into(),
                PlanKind::Write => {
                    let what = match s.mode {
                        Mode::Firmware => format!("INSTALL {}", s.file(FileKind::Firmware).unwrap_or_default().trim_end_matches(".fwsc")),
                        Mode::Restore => format!("RESTORE {}", s.file(FileKind::Restore).unwrap_or_default().trim_end_matches(".bin")),
                        _ => match s.boot_source {
                            BootSource::Open => format!("M-BOOTLOADER {}", s.open_version),
                            BootSource::Stock => "STOCK BOOTLOADER".into(),
                            BootSource::File => format!("BOOTLOADER {}", s.file(FileKind::BootFile).unwrap_or_default()),
                        },
                    };
                    let line = format!("{what} · {} SECTORS ({} KiB) · {}", p.count, p.count * 4, p.short);
                    if line.chars().count() <= 60 { line } else { format!("{what} · {} SECTORS · {}", p.count, p.short) }
                }
            },
        },
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let st = self.ctl.snapshot();
        let ctx = ui.ctx().clone();
        let screen = ctx.content_rect();
        let s = (screen.width() / W).min(screen.height() / H);
        let board = Board { s, o: screen.center() - vec2(W * s, H * s) / 2.0 };
        let painter = ui.painter().clone();
        // letters the fonts lack, from the texts drawn: looked for after the frame
        let fonts = std::mem::take(&mut self.fonts);
        let missing = RefCell::new(vec![]);
        let p = Painter { p: &painter, b: &board, fonts: &fonts, missing: &missing };
        let noise = self.noise.get_or_insert_with(|| noise_texture(&ctx)).clone();
        let dev = st.device.as_ref();
        let busy = st.busy;
        let modal = self.dialog != Dialog::None || st.alert.is_some();
        let active = !modal && !busy; // clicks allowed
        let clicked = |id: &str, r: Rect| -> (bool, bool) {
            let resp = ui.interact(r, ui.id().with(id), Sense::click());
            (resp.clicked(), resp.hovered())
        };

        // ---- background ----
        p.rect(screen, BG2);
        let board_rect = board.r(0.0, 0.0, W, H);
        p.vgrad(board_rect, hex(0x2d2622), BG2);
        let mut m = Mesh::with_texture(noise.id());
        m.add_rect_with_uv(board_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(W / 160.0, H / 160.0)), Color32::WHITE);
        painter.add(Shape::mesh(m));

        // ---- masthead ----
        let origin = board.p(40.0, 116.0);
        let ww = crate::wordmark::draw(&painter, origin.x, origin.y, 72.0 * s, MINT) / s;
        p.text(38.0 + ww + 24.0, 70.0, Align2::LEFT_CENTER, "USB FIRMWARE UTILITY", "tech", 21.0, LAV, 2.4);
        p.text(38.0 + ww + 24.0, 96.0, Align2::LEFT_CENTER, "FOR THE FM-1 AND SMK-37", "tech", 14.0, hex(0x8f80b6), 3.0);
        p.text(1500.0, 80.0, Align2::RIGHT_CENTER, &format!("v{}", env!("CARGO_PKG_VERSION")), "tech", 14.0, LAV, 1.0);
        let aw = p.text(1420.0, 80.0, Align2::RIGHT_CENTER, "ABOUT", "tech", 17.0, hex(0xa9d8e6), 2.0);
        p.text(1420.0 - aw - 14.0, 80.0, Align2::RIGHT_CENTER, "|", "tech", 17.0, LAVLINE, 0.0);
        let bw = p.text(1420.0 - aw - 34.0, 80.0, Align2::RIGHT_CENTER, "BACKUPS", "tech", 17.0, hex(0xa9d8e6), 2.0);
        if clicked("about", board.r(1420.0 - aw, 68.0, aw, 24.0)).0 && !modal {
            self.dialog = Dialog::About;
        }
        if clicked("backups", board.r(1420.0 - aw - 34.0 - bw, 68.0, bw, 24.0)).0 && !modal {
            self.ctl.open_backups();
        }
        p.hline(36.0, 1500.0, 128.0, LAVLINE);
        p.vline(913.0, 142.0, 988.0, LAVLINE);

        // ---- LCD ----
        p.label(38.0, 148.0, "CONNECTED DEVICE", None);
        let bezel = board.r(38.0, 158.0, 853.0, 212.0);
        p.rect(bezel, hex(0x141110));
        p.frame(bezel, hex(0x3a322d), 1.0);
        let lcd = board.r(62.0, 173.0, 805.0, 182.0);
        p.vgrad(lcd, LCD1, LCD2);
        p.frame(lcd, hex(0x77913a), 2.0);
        for y in (0..182).step_by(4) {
            p.hline(62.0, 867.0, 173.0 + y as f32, Color32::from_black_alpha(9));
        }
        lcd_icon(&p, 92.0, 196.0, dev.is_some());
        p.vline(346.0, 185.0, 312.0, LCDINK);
        p.hline(84.0, 845.0, 317.0, LCDINK);
        let (l1, l2, l3) = match dev {
            Some(d) => (d.model.replace(" (probably)", "").to_uppercase(),
                        if d.certain { "USB CONNECTED · DOWNLOAD MODE".to_string() } else { "MODEL NOT CERTAIN · CHECK BELOW".into() },
                        "FLASH 1 MiB".to_string()),
            None if st.phase == Phase::Connecting => ("READING".into(), "FLASH READ · CHECKING".into(), format!("{}%", (st.pct * 100.0) as u32)),
            None => ("NO UNIT".into(), if st.multiple { "MORE THAN ONE UNIT".into() } else { "WAITING FOR DOWNLOAD MODE".into() }, "USB".into()),
        };
        p.text_fit(372.0, 214.0, &l1, "lcd", 58.0, 470.0, LCDINK, 2.0);
        p.text_fit(372.0, 254.0, &l2, "lcd", 30.0, 470.0, LCDINK, 1.5);
        p.hline(372.0, 845.0, 272.0, LCDINK);
        p.text(372.0, 293.0, Align2::LEFT_CENTER, &l3, "lcd", 30.0, LCDINK, 1.5);
        let line = prompt(&st);
        let alert_ink = st.plan.as_ref().is_some_and(|p| p.kind == PlanKind::Refused);
        let mut ink = if alert_ink { hex(0x5b1206) } else { LCDINK };
        // after an inactive key: the line shakes, and flashes inverted three times
        let now = ctx.input(|i| i.time);
        let t = now - self.nudged;
        let mut dx = 0.0;
        if (0.0..NUDGE).contains(&t) {
            dx = (t * 45.0).sin() as f32 * 5.0 * (1.0 - t as f32 / 0.45).max(0.0);
            if ((t / 0.2) as i64) % 2 == 0 {
                p.rect(board.r(76.0, 323.0, 784.0, 28.0), ink);
                ink = LCD1;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        let pw = p.text_fit(84.0 + dx, 337.0, &line, "lcd", 29.0, 740.0, ink, 1.4);
        if (now * 2.0) as i64 % 2 == 0 {
            p.rect(board.r(84.0 + dx + pw + 8.0, 326.0, 13.0, 22.0), ink);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        // ---- device info ----
        p.label(38.0, 388.0, "DEVICE INFO", Some(891.0));
        let dash = "–".to_string();
        let rows: [[(&str, String, Color32); 2]; 3] = match dev {
            Some(d) => [
                [("MODEL", d.model.to_uppercase(), if d.certain { INK } else { WARN }), ("BOOTLOADER", d.bootloader.clone(), INK)],
                [("FIRMWARE", d.firmware.clone(), INK), ("FLASH CHIP", format!("{} · 1 MiB", d.flash), INK)],
                [("FIRMWARE CHECK", d.app.clone(), if d.app_ok { MINT } else { BAD }), ("CHIP KEY", d.chip_key.clone(), if d.chip_key_ok { INK } else { WARN })],
            ],
            None => [
                [("MODEL", dash.clone(), INK), ("BOOTLOADER", dash.clone(), INK)],
                [("FIRMWARE", dash.clone(), INK), ("FLASH CHIP", dash.clone(), INK)],
                [("FIRMWARE CHECK", dash.clone(), INK), ("CHIP KEY", dash.clone(), INK)],
            ],
        };
        for (i, row) in rows.iter().enumerate() {
            let y = 413.0 + i as f32 * 29.0;
            for (j, (k, v, c)) in row.iter().enumerate() {
                let x = if j == 0 { 38.0 } else { 484.0 };
                p.text(x + 10.0, y, Align2::LEFT_CENTER, k, "tech", 14.0, LAV, 1.6);
                p.text_fit(x + 160.0, y, v, "plex", 15.0, 240.0, *c, 0.0);
                p.hline(x, x + 407.0, y + 14.5, hex(0x3c3433));
            }
        }
        p.vline(465.0, 402.0, 484.0, LAVLINE);
        if let Some(d) = dev {
            if !d.certain {
                let r = board.r(48.0, 400.0, 400.0, 26.0);
                let (c, h) = clicked("model", r);
                if h && active {
                    painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(s, WARN), StrokeKind::Inside);
                }
                p.text(440.0, 413.0, Align2::RIGHT_CENTER, "CHOOSE ▸", "tech", 12.0, WARN, 1.0);
                if c && active {
                    self.dialog = Dialog::Model;
                }
            }
        }

        // ---- operation keys ----
        p.label(38.0, 510.0, "OPERATIONS", Some(891.0));
        let keys = [
            (Mode::Backup, "BACKUP", "FULL FLASH", Icon::Backup),
            (Mode::Restore, "RESTORE", "FULL FLASH", Icon::Restore),
            (Mode::Firmware, "FLASH", "FIRMWARE", Icon::Chip),
            (Mode::Bootloader, "FLASH", "BOOTLOADER", Icon::Boot),
        ];
        let kw = (853.0 - 3.0 * 14.0) / 4.0;
        for (i, (mode, t1, t2, icon)) in keys.iter().enumerate() {
            let x = 38.0 + i as f32 * (kw + 14.0);
            let r = board.r(x, 524.0, kw, 72.0);
            let (c, h) = clicked(&format!("op{i}"), r); // ids must be unique: two keys say FULL FLASH
            let on = dev.is_some() && st.mode == *mode;
            let enabled = dev.is_some() && active;
            key(&p, r, on, h && enabled, !enabled && !on);
            draw_icon(&p, *icon, x + 22.0, 543.0, 34.0, KEYINK);
            p.text(x + 70.0, 549.0, Align2::LEFT_CENTER, t1, "cond", 21.0, KEYINK, 1.6);
            p.text(x + 70.0, 571.0, Align2::LEFT_CENTER, t2, "cond", 21.0, KEYINK, 1.6);
            led(&p, board.p(x + kw - 18.0, 542.0), on);
            if c && enabled {
                self.ctl.select_mode(*mode);
            } else if c && !modal {
                self.nudged = now;
            }
        }

        // ---- operation panel ----
        let title = match (dev, st.mode) {
            (None, _) => "HOW TO CONNECT",
            (_, Mode::Backup) => "BACKUP DESTINATION",
            (_, Mode::Restore) => "BACKUP FILE",
            (_, Mode::Firmware) => "FIRMWARE FILE",
            (_, Mode::Bootloader) => "BOOTLOADER SOURCE",
        };
        p.label(38.0, 630.0, title, Some(891.0));
        p.frame(board.r(38.0, 640.0, 853.0, 146.0), LAVLINE, 1.0);
        let file_row = |p: &Painter, label: &str, value: Option<String>, button: &str, id: &str| -> bool {
            p.text(56.0, 674.0, Align2::LEFT_CENTER, label, "tech", 14.0, LAV, 1.5);
            let field = board.r(200.0, 655.0, 514.0, 38.0);
            p.rect(field, hex(0x1d1917));
            p.frame(field, hex(0x5a5060), 1.0);
            let (txt, col) = match value { Some(v) => (v, INK), None => ("no file chosen".into(), hex(0x5c534d)) };
            p.text_fit(214.0, 674.0, &txt, "plex", 17.0, 490.0, col, 0.0);
            let br = board.r(728.0, 655.0, 146.0, 38.0);
            let (c, h) = clicked(id, br);
            key(p, br, false, h && active, !active);
            p.text(801.0, 674.0, Align2::CENTER_CENTER, button, "cond", 18.0, KEYINK, 1.6);
            c && active
        };
        let auto_line = match &st.backup_saved {
            Some(n) => format!("AUTO BACKUP SAVED · {n}"),
            None => "AUTO BACKUP BEFORE EVERY WRITE".into(),
        };
        match (dev, st.mode) {
            (None, _) if st.midi.is_some() => {
                let name = st.midi.clone().unwrap_or_default();
                if file_row(&p, "USB MIDI", Some(format!("{name} · running")), "DOWNLOAD MODE", "softkey") {
                    self.ctl.enter_download_mode();
                }
                p.text(56.0, 720.0, Align2::LEFT_CENTER,
                    "Restarts it into download mode with a MIDI message: no keys, whichever bootloader it has.",
                    "tech", 15.0, DIM, 0.3);
                p.text(56.0, 744.0, Align2::LEFT_CENTER,
                    "Nothing is written. Switching it off and on leaves download mode.", "tech", 15.0, DIM, 0.3);
            }
            (None, _) => {
                let mut rows = vec![
                    ("M-VAVE firmware or Felucca running", "connect it: DOWNLOAD MODE appears here"),
                    ("SMK-37 with m-bootloader", "hold SEQ REC while switching on"),
                    ("FM-1 with m-bootloader", "hold REC while switching on"),
                    ("Felucca, SLOOP, Snoop37 running", "hold OCT- and OCT+ for 5 s"),
                    ("Then", "connect USB: a data cable, no hub"),
                ];
                if cfg!(target_os = "macos") {
                    rows.push(("Removable volume prompt", "choose Allow: that is the unit"));
                }
                let step = if rows.len() > 5 { 21.0 } else { 25.0 };
                for (i, (a, b)) in rows.iter().enumerate() {
                    let y = 662.0 + i as f32 * step;
                    p.text(56.0, y, Align2::LEFT_CENTER, a, "tech", 15.5, LAV, 0.0);
                    p.text(360.0, y, Align2::LEFT_CENTER, b, "tech", 15.5, INK, 0.0);
                }
            }
            (_, Mode::Backup) => {
                let dir = st.backup_dir.display().to_string();
                if file_row(&p, "FOLDER", Some(dir), "OPEN FOLDER", "openfolder") {
                    self.ctl.open_backups();
                }
                p.text(56.0, 720.0, Align2::LEFT_CENTER,
                    "Saves the whole flash (1 MiB, read and checked when the unit connected) with a .json describing it.",
                    "tech", 15.0, DIM, 0.3);
                p.text(56.0, 744.0, Align2::LEFT_CENTER, "Nothing is written to the unit.", "tech", 15.0, DIM, 0.3);
            }
            (_, Mode::Restore) => {
                if file_row(&p, "BACKUP FILE", st.file(FileKind::Restore), "CHOOSE FILE", "chooserestore") {
                    self.ctl.choose_file(FileKind::Restore);
                }
                let chosen = st.file(FileKind::Restore);
                for (i, b) in st.backups.iter().take(2).enumerate() {
                    let y = 714.0 + i as f32 * 24.0;
                    let r = board.r(50.0, y - 11.0, 820.0, 22.0);
                    let (c, h) = clicked(&b.name, r);
                    let sel = chosen.as_deref() == Some(b.name.as_str());
                    if h && active {
                        p.rect(r, hex(0x312a26));
                    }
                    if sel || (h && active) {
                        p.rect(board.r(50.0, y - 11.0, 2.0, 22.0), MINT);
                    }
                    let col = if sel { MINT } else { INK };
                    p.text(58.0, y, Align2::LEFT_CENTER, &b.name, "plex", 14.0, col, 0.0);
                    p.text(370.0, y, Align2::LEFT_CENTER, &b.time, "plex", 14.0, DIM, 0.0);
                    p.text_fit(530.0, y, &b.firmware, "plex", 14.0, 330.0, DIM, 0.0);
                    if c && active {
                        self.ctl.pick_backup(b.path.clone());
                    }
                }
                p.text(56.0, 772.0, Align2::LEFT_CENTER, &auto_line, "tech", 13.5, LAV, 2.0);
            }
            (_, Mode::Firmware) => {
                if file_row(&p, "FIRMWARE FILE", st.file(FileKind::Firmware), "CHOOSE FILE", "choosefw") {
                    self.ctl.choose_file(FileKind::Firmware);
                }
                if checkbox(ui, &p, 56.0, 713.0, "Also erase settings and Bluetooth pairings", st.wipe, active) {
                    self.ctl.set_wipe(!st.wipe);
                }
                if checkbox(ui, &p, 56.0, 743.0, "Also put back the package's stock bootloader", st.with_boot, active) {
                    self.ctl.set_with_boot(!st.with_boot);
                }
                p.text(56.0, 772.0, Align2::LEFT_CENTER, &auto_line, "tech", 13.5, LAV, 2.0);
            }
            (_, Mode::Bootloader) => {
                p.text(56.0, 674.0, Align2::LEFT_CENTER, "SOURCE", "tech", 14.0, LAV, 1.5);
                let mut x = 200.0;
                let opts = [
                    (BootSource::Open, format!("M-BOOTLOADER {}", st.open_version).to_uppercase()),
                    (BootSource::Stock, "STOCK (FROM .FWSC)".into()),
                    (BootSource::File, "FILE (.BOOT)".into()),
                ];
                for (src, t) in opts.iter() {
                    let w = t.len() as f32 * 9.5 + 36.0;
                    let r = board.r(x, 655.0, w, 38.0);
                    let (c, h) = clicked(t, r);
                    key(&p, r, st.boot_source == *src, h && active, !active);
                    p.text(x + w / 2.0, 674.0, Align2::CENTER_CENTER, t, "cond", 17.0, KEYINK, 1.4);
                    if c && active {
                        self.ctl.set_boot_source(*src);
                    }
                    x += w + 10.0;
                }
                match st.boot_source {
                    BootSource::Open => {
                        p.text(56.0, 722.0, Align2::LEFT_CENTER,
                            &format!("The bundled m-bootloader {} for the identified model.", st.open_version), "tech", 15.0, DIM, 0.3);
                        p.text(56.0, 746.0, Align2::LEFT_CENTER, "Writes the boot area only (16 KiB).", "tech", 15.0, DIM, 0.3);
                    }
                    src => {
                        let (lab, kind) = if src == BootSource::Stock { ("PACKAGE", FileKind::Stock) } else { ("BOOTLOADER", FileKind::BootFile) };
                        let v = st.file(kind);
                        p.text(56.0, 727.0, Align2::LEFT_CENTER, lab, "tech", 14.0, LAV, 1.5);
                        let field = board.r(200.0, 708.0, 514.0, 38.0);
                        p.rect(field, hex(0x1d1917));
                        p.frame(field, hex(0x5a5060), 1.0);
                        let (txt, col) = match v { Some(v) => (v, INK), None => ("no file chosen".into(), hex(0x5c534d)) };
                        p.text_fit(214.0, 727.0, &txt, "plex", 17.0, 490.0, col, 0.0);
                        let br = board.r(728.0, 708.0, 146.0, 38.0);
                        let (c, h) = clicked("choosebootfile", br);
                        key(&p, br, false, h && active, !active);
                        p.text(801.0, 727.0, Align2::CENTER_CENTER, "CHOOSE FILE", "cond", 18.0, KEYINK, 1.6);
                        if c && active {
                            self.ctl.choose_file(kind);
                        }
                    }
                }
                p.text(56.0, 772.0, Align2::LEFT_CENTER, &auto_line, "tech", 13.5, LAV, 2.0);
            }
        }

        // ---- primary key ----
        let pr = board.r(38.0, 796.0, 853.0, 62.0);
        let (pc, ph) = clicked("primary", pr);
        let ready = dev.is_some() && st.plan.as_ref().is_some_and(|p| p.ready) && st.phase != Phase::Done;
        let label = match (st.phase, st.mode) {
            (Phase::Connecting, _) => "READING…",
            (Phase::Working, Mode::Backup) => "SAVING…",
            (Phase::Working, _) => "WRITING…",
            (Phase::Done, _) => "DONE",
            (_, Mode::Backup) => "SAVE FULL BACKUP",
            (_, Mode::Restore) => "REVIEW & RESTORE",
            _ => "REVIEW & FLASH",
        };
        key(&p, pr, false, ph && active && ready, !(active && ready));
        let lw = p.measure(label, "cond", 32.0, 3.0);
        let lx = 38.0 + (853.0 - lw - 48.0) / 2.0;
        draw_icon(&p, Icon::Bolt, lx, 812.0, 30.0, MINT);
        p.text(lx + 48.0, 827.0, Align2::LEFT_CENTER, label, "cond", 32.0, KEYINK, 3.0);
        if pc && active && ready {
            if st.mode == Mode::Backup {
                self.ctl.save_backup();
            } else {
                self.dialog = Dialog::Review;
            }
        } else if pc && !modal {
            self.nudged = now;
        }

        // ---- transfer progress ----
        p.label(38.0, 884.0, "TRANSFER PROGRESS", Some(470.0));
        p.frame(board.r(38.0, 900.0, 432.0, 96.0), hex(0x50465a), 1.0);
        let sw = 408.0 / 4.0;
        for (i, name) in ["READ", "BACKUP", "WRITE", "VERIFY"].iter().enumerate() {
            let x = 50.0 + i as f32 * sw;
            let pts: Vec<Pos2> = [(0.0, 0.0), (sw - 12.0, 0.0), (sw, 16.0), (sw - 12.0, 32.0), (0.0, 32.0), (if i == 0 { 0.0 } else { 12.0 }, 16.0)]
                .iter().map(|(dx, dy)| board.p(x + dx, 912.0 + dy)).collect();
            let (fill, ink) = match st.stage[i] {
                Stage::Done => (hex(0x2b4d3f), MINT),
                Stage::Active => (MINT, hex(0x102a20)),
                Stage::Idle => (hex(0x2a2a28), hex(0xb7b0aa)),
            };
            painter.add(Shape::convex_polygon(pts, fill, Stroke::NONE));
            let tw = p.text(x + sw / 2.0 + 3.0, 928.0, Align2::CENTER_CENTER, name, "cond", 15.0, ink, 1.6);
            if st.stage[i] == Stage::Done {
                let cx = x + sw / 2.0 + 3.0 - tw / 2.0 - 15.0;
                painter.add(Shape::line(vec![board.p(cx, 928.0), board.p(cx + 4.0, 932.0), board.p(cx + 11.0, 923.0)], Stroke::new(2.0 * s, ink)));
            }
        }
        let bar = board.r(50.0, 958.0, 348.0, 20.0);
        p.frame(bar, hex(0x6c6464), 1.0);
        p.rect(board.r(52.0, 960.0, 344.0 * st.pct.clamp(0.0, 1.0), 16.0), MINT);
        p.text(458.0, 968.0, Align2::RIGHT_CENTER, &format!("{}%", (st.pct * 100.0) as u32), "tech", 19.0, MINT, 0.0);

        // ---- log ----
        p.label(496.0, 884.0, "OPERATION LOG", Some(840.0));
        p.text(891.0, 884.0, Align2::RIGHT_CENTER, "CLEAR", "tech", 12.0, hex(0xa9d8e6), 1.5);
        if clicked("clearlog", board.r(850.0, 874.0, 42.0, 20.0)).0 && !modal {
            self.ctl.clear_log();
        }
        p.frame(board.r(496.0, 900.0, 395.0, 96.0), hex(0x50465a), 1.0);
        let clip = painter.with_clip_rect(board.r(506.0, 900.0, 378.0, 96.0));
        let lines: Vec<&String> = st.log.iter().rev().take(4).collect();
        for (i, l) in lines.iter().rev().enumerate() {
            let col = if l.contains("Refused") || l.contains("error") || l.contains("gone") { WARN } else { hex(0xcfeee2) };
            p.note("plex", l);
            clip.text(board.p(508.0, 920.0 + i as f32 * 19.0), Align2::LEFT_CENTER, l.as_str(), board.f("plex", 13.0), col);
        }

        // ---- flash map ----
        self.flash_map(ui, &p, &st);

        // ---- dialogs ----
        self.dialogs(ui, &p, &st, &line);

        let mut fonts = fonts;
        let missing = missing.into_inner();
        if !missing.is_empty() && fonts.search(&missing) {
            fonts.install(&ctx);
            ctx.request_repaint();
        }
        self.fonts = fonts;
    }
}

impl App {
    fn flash_map(&mut self, ui: &egui::Ui, p: &Painter, st: &Shared) {
        let (board, s, painter) = (p.b, p.b.s, p.p);
        let dev = st.device.as_ref();
        p.label(938.0, 148.0, "FLASH MAP", Some(1310.0));
        p.text(1500.0, 148.0, Align2::RIGHT_CENTER, "1 MiB = 256 × 4 KiB", "tech", 15.0, LAV, 1.0);
        let (gx, gy, gw, gh, pad, gap) = (972.0, 198.0, 528.0, 532.0, 4.0, 5.0);
        let cw = (gw - 2.0 * pad - 15.0 * gap) / 16.0;
        let ch = (gh - 2.0 * pad - 15.0 * gap) / 16.0;
        for c in 0..16 {
            p.text(gx + pad + c as f32 * (cw + gap) + cw / 2.0, 187.0, Align2::CENTER_CENTER, &format!("{c:X}"), "tech", 16.0, INK, 0.0);
        }
        let writing = st.phase == Phase::Working && st.mode != Mode::Backup;
        let dimmed = !st.marks.is_empty();
        let mut hovered = None;
        for i in 0..256 {
            let (row, col) = (i / 16, i % 16);
            if col == 0 {
                p.text(938.0, gy + pad + row as f32 * (ch + gap) + ch / 2.0, Align2::LEFT_CENTER, &format!("{:02X}", row * 16), "tech", 15.0, INK, 0.0);
            }
            let r = board.r(gx + pad + col as f32 * (cw + gap), gy + pad + row as f32 * (ch + gap), cw, ch);
            if ui.rect_contains_pointer(r) {
                hovered = Some(i);
            }
            let region = st.map[i];
            let mut mark = st.marks.get(&i).copied();
            let fade = if dev.is_none() { 0.38 } else if dimmed && mark.is_none() { 0.42 } else { 1.0 };
            let mut outline = None;
            let mut fill = region_color(region);
            let mut solid = region != Region::Free;
            if writing && mark == Some(Mark::Change) {
                mark = None;
                fill = hex(0x26302d);
                outline = Some(hex(0x4f6a62));
                solid = true;
            }
            match mark {
                Some(Mark::Change) => outline = Some(hex(0xf4efe4)),
                Some(Mark::Now) => {
                    fill = NOW;
                    solid = true;
                    painter.rect_filled(r.expand(4.0 * s), CornerRadius::same(3), NOW.gamma_multiply(0.25));
                }
                Some(Mark::Written) => {
                    fill = hex(0xb7fff0);
                    solid = true;
                }
                Some(Mark::Verified) => {
                    fill = hex(0x8ff5df);
                    outline = Some(hex(0x2c8f78));
                    solid = true;
                }
                None => {}
            }
            if solid {
                p.rect(r, fill.gamma_multiply(fade));
                painter.line_segment([r.left_top(), r.right_top()], Stroke::new(s, Color32::from_white_alpha((40.0 * fade) as u8)));
            } else {
                hatch(p, r, fade);
            }
            if region == Region::Calibration && mark.is_none() {
                lock(p, r);
            }
            if let Some(c) = outline {
                painter.rect_stroke(r.expand(1.5 * s), CornerRadius::ZERO, Stroke::new(2.0 * s, c), StrokeKind::Outside);
            }
            if hovered == Some(i) && dev.is_some() {
                painter.rect_stroke(r.expand(2.0 * s), CornerRadius::ZERO, Stroke::new(2.0 * s, LAV), StrokeKind::Outside);
            }
        }

        // legend: the regions this unit has, with their sectors
        let mut idx = 0;
        for reg in Region::LEGEND {
            let ranges = region_ranges(&st.map, reg);
            if dev.is_some() && ranges.is_empty() {
                continue;
            }
            let x = 938.0 + (idx % 3) as f32 * 190.0;
            let y = 762.0 + (idx / 3) as f32 * 24.0;
            idx += 1;
            let sq = board.r(x, y - 9.5, 19.0, 19.0);
            if reg == Region::Free { hatch(p, sq, 1.4) } else { p.rect(sq, region_color(reg)) }
            let w = p.text(x + 29.0, y, Align2::LEFT_CENTER, reg.name(), "tech", 15.0, INK, 1.2);
            if dev.is_some() {
                p.text_fit(x + 29.0 + w + 8.0, y, &ranges, "tech", 13.5, 150.0 - w, DIM, 0.0);
            }
        }
        p.hline(938.0, 1500.0, 848.0, LAVLINE);
        let n = st.marks.len();
        let marks: Vec<(Color32, Option<Color32>, String)> = if writing {
            vec![(hex(0xb7fff0), None, "WRITTEN".into()), (NOW, None, "NOW".into()),
                 (hex(0x26302d), Some(hex(0x4f6a62)), "PENDING".into()), (hex(0x8ff5df), Some(hex(0x2c8f78)), "VERIFIED".into())]
        } else if st.phase == Phase::Done && n > 0 {
            vec![(hex(0x8ff5df), Some(hex(0x2c8f78)), format!("WRITTEN AND VERIFIED · {n}"))]
        } else if n > 0 {
            vec![(Color32::TRANSPARENT, Some(hex(0xf4efe4)), format!("WILL CHANGE · {n}")), (hex(0x3b4a45), None, "UNTOUCHED".into())]
        } else {
            vec![]
        };
        let mut x = 938.0;
        for (fill, ol, t) in &marks {
            let sq = board.r(x, 862.0, 19.0, 19.0);
            p.rect(sq, *fill);
            if let Some(c) = ol { p.frame(sq, *c, 2.0) }
            let w = p.text(x + 29.0, 872.0, Align2::LEFT_CENTER, t, "tech", 15.0, INK, 1.5);
            x += 29.0 + w + 36.0;
        }
        if marks.is_empty() && dev.is_some() {
            let t = if st.mode == Mode::Backup { "A BACKUP READS ALL 256 SECTORS" } else { "NOTHING MARKED: CHOOSE A FILE TO SEE WHAT CHANGES" };
            p.text(938.0, 872.0, Align2::LEFT_CENTER, t, "tech", 14.0, DIM, 1.2);
        }
        if let (Some(i), Some(_)) = (hovered, dev) {
            let what = match st.marks.get(&i) {
                Some(Mark::Change) => " · will change",
                Some(Mark::Written) => " · written",
                Some(Mark::Verified) => " · written and verified",
                Some(Mark::Now) => " · writing now",
                None => "",
            };
            p.text(938.0, 906.0, Align2::LEFT_CENTER,
                &format!("SECTOR {:02X} · 0x{:05X}-0x{:05X} · {}{what}", i, i * 0x1000, i * 0x1000 + 0xFFF, st.map[i].name()),
                "plex", 14.0, DIM, 0.0);
        }
    }

    fn dialogs(&mut self, ui: &egui::Ui, p: &Painter, st: &Shared, line: &str) {
        let (board, painter) = (p.b, p.p);
        let ctl = self.ctl.clone();
        // an alert from the controller wins over the user's dialogs
        let (title, body, keys): (&str, Vec<(String, Color32)>, Vec<(&str, u8)>) = if let Some((kind, text)) = &st.alert {
            let (t, c) = match kind {
                AlertKind::Done => ("DONE", MINT),
                AlertKind::Refused => ("REFUSED", BAD),
                AlertKind::Error => ("ERROR", BAD),
            };
            let mut t2 = text.clone();
            if let Some(f) = t2.get_mut(0..1) { f.make_ascii_uppercase(); }
            (t, t2.split('\n').map(|l| (l.to_string(), c)).collect(), vec![("OK", 1)])
        } else {
            match self.dialog {
                Dialog::None => return,
                Dialog::Review => {
                    let Some(plan) = st.plan.as_ref().filter(|p| p.ready && st.phase == Phase::Ready) else {
                        self.dialog = Dialog::None;
                        return;
                    };
                    let mut b = vec![(format!("{}: writes {} sectors ({} KiB): {}", plan.title, plan.count, plan.count * 4, plan.ranges), INK)];
                    b.extend(plan.notes.iter().filter(|n| !n.starts_with("Installs ")).map(|n| (n.clone(), INK)));
                    b.extend(plan.warnings.iter().map(|w| (format!("⚠ {w}"), WARN)));
                    b.push((match &st.backup_saved {
                        Some(n) => format!("Backup of this session: {n}; everything written is read back and verified."),
                        None => "A full backup of the unit is saved first; everything written is read back and verified.".into(),
                    }, MINT));
                    b.push(("Do not unplug or switch off the unit while it writes.".into(), BAD));
                    ("REVIEW", b, vec![("CANCEL", 0), ("WRITE", 2)])
                }
                Dialog::About => ("ABOUT", vec![
                    (format!("M-Flasher {}: back up, restore and reinstall the firmware of the M-VAVE FM-1 and SMK-37 \
                              Pro / Elite through the chip's USB download mode, and install m-bootloader.",
                             env!("CARGO_PKG_VERSION")), INK),
                    (String::new(), INK),
                    ("Bootloader and flash loader: m-bootloader (MIT). Protocol after jl-uboot-tool by \
                      kagaimiq (MIT). Fonts: Saira, Barlow Condensed, Share Tech Mono, IBM Plex Mono, VT323 \
                      (OFL).".into(), DIM),
                    (String::new(), INK),
                    ("Unofficial: not affiliated with M-VAVE or JieLi.".into(), DIM),
                ], vec![("CLOSE", 0)]),
                Dialog::Model => {
                    let mut b = vec![
                        ("The model could not be identified for certain. Writing the wrong model's firmware or".to_string(), INK),
                        ("bootloader can make the unit unusable: choose only if you are sure. Found:".into(), WARN),
                    ];
                    let ev = st.device.as_ref().map(|d| d.evidence.clone()).unwrap_or_default();
                    b.extend(ev.into_iter().map(|e| (format!("· {e}"), DIM)));
                    ("WHICH MODEL IS THIS?", b, vec![("CANCEL", 0), ("FM-1", 3), ("SMK-37", 4)])
                }
            }
        };
        let screen = painter.clip_rect();
        painter.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(160));
        // wrapped to the dialog's text width, measured in the font itself
        let fits = |l: &str| p.measure(l, "tech", 16.5, 0.2) <= 690.0;
        let wrapped: Vec<(String, Color32)> = body.into_iter().flat_map(|(t, c)| wrap(&t, &fits).into_iter().map(move |l| (l, c))).collect();
        let review = self.dialog == Dialog::Review && st.alert.is_none();
        let h = 150.0 + wrapped.len() as f32 * 27.0 + if review { 66.0 } else { 0.0 };
        let top = (H - h) / 2.0;
        let d = board.r(388.0, top, 760.0, h);
        p.vgrad(d, hex(0x302925), hex(0x241f1c));
        p.frame(d, hex(0x6d5f85), 1.0);
        p.label(420.0, top + 32.0, title, Some(1116.0));
        let mut y = top + 64.0;
        if review {
            let lcd = board.r(420.0, y, 696.0, 52.0);
            p.vgrad(lcd, LCD1, LCD2);
            p.text_fit(436.0, y + 26.0, line, "lcd", 28.0, 664.0, LCDINK, 1.0);
            y += 66.0;
        }
        for (l, c) in &wrapped {
            p.text(420.0, y + 12.0, Align2::LEFT_CENTER, l, "tech", 16.5, *c, 0.2);
            y += 27.0;
        }
        let mut x = 1116.0;
        for (label, code) in keys.iter().rev() {
            let w = if *code == 2 { 254.0 } else { 150.0 };
            x -= w;
            let r = board.r(x, top + h - 78.0, w, 52.0);
            let resp = ui.interact(r, ui.id().with(("dlg", *label)), Sense::click());
            key(p, r, *code == 2, resp.hovered(), false);
            let t = if *code == 2 { format!("WRITE {} SECTORS", st.plan.as_ref().map(|p| p.count).unwrap_or(0)) } else { label.to_string() };
            p.text(x + w / 2.0, top + h - 52.0, Align2::CENTER_CENTER, &t, "cond", 22.0, KEYINK, 1.6);
            if resp.clicked() {
                if st.alert.is_some() {
                    ctl.dismiss();
                } else {
                    match code {
                        2 => ctl.confirm(),
                        3 => ctl.set_model("fm1"),
                        4 => ctl.set_model("smk37"),
                        _ => {}
                    }
                    self.dialog = Dialog::None;
                }
            }
            x -= 16.0;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) && st.alert.is_none() {
            self.dialog = Dialog::None;
        }
    }
}

/// Lines of text, broken at spaces so that each one fits; an empty text is one
/// empty line.
fn wrap(text: &str, fits: &dyn Fn(&str) -> bool) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = vec![];
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && !fits(&format!("{line} {word}")) {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// The window's icon: macOS's with the margin its icons have, the others' without.
const ICON: &[u8] = if cfg!(target_os = "macos") {
    include_bytes!("../assets/icon-256.png")
} else {
    include_bytes!("../linux/m-flasher.png")
};

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("M-Flasher")
            // Linux: matches the installed launcher, for the dock's icon
            .with_app_id("io.github.jvitkauskas.m-flasher")
            .with_inner_size(vec2(1280.0, 853.0))
            .with_min_inner_size(vec2(900.0, 600.0))
            // ours (macos/icon.svg), not egui's: the Dock and taskbars show it
            .with_icon(eframe::icon_data::from_png_bytes(ICON).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native("M-Flasher", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

// ---- drawing helpers (board coordinates, text, keys, icons) ----

/// Board coordinates -> screen.
struct Board {
    s: f32,
    o: Pos2,
}

impl Board {
    fn p(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.o.x + x * self.s, self.o.y + y * self.s)
    }
    fn r(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.p(x, y), vec2(w * self.s, h * self.s))
    }
    fn f(&self, family: &str, size: f32) -> FontId {
        FontId::new(size * self.s, FontFamily::Name(family.into()))
    }
}

struct Painter<'a> {
    p: &'a egui::Painter,
    b: &'a Board,
    fonts: &'a crate::fonts::Fonts,
    missing: &'a RefCell<Vec<char>>,
}

impl Painter<'_> {
    /// Notes the letters of a text that the fonts lack.
    fn note(&self, family: &str, s: &str) {
        self.fonts.missing(family, s, &mut self.missing.borrow_mut());
    }
    fn rect(&self, r: Rect, fill: Color32) {
        self.p.rect_filled(r, CornerRadius::ZERO, fill);
    }
    fn frame(&self, r: Rect, color: Color32, width: f32) {
        self.p.rect_stroke(r, CornerRadius::ZERO, Stroke::new(width * self.b.s, color), StrokeKind::Inside);
    }
    fn vgrad(&self, r: Rect, top: Color32, bottom: Color32) {
        let mut m = Mesh::default();
        let i = m.vertices.len() as u32;
        m.colored_vertex(r.left_top(), top);
        m.colored_vertex(r.right_top(), top);
        m.colored_vertex(r.right_bottom(), bottom);
        m.colored_vertex(r.left_bottom(), bottom);
        m.add_triangle(i, i + 1, i + 2);
        m.add_triangle(i, i + 2, i + 3);
        self.p.add(Shape::mesh(m));
    }
    fn hline(&self, x0: f32, x1: f32, y: f32, color: Color32) {
        let b = self.b;
        self.p.line_segment([b.p(x0, y), b.p(x1, y)], Stroke::new(b.s.max(1.0), color));
    }
    fn vline(&self, x: f32, y0: f32, y1: f32, color: Color32) {
        let b = self.b;
        self.p.line_segment([b.p(x, y0), b.p(x, y1)], Stroke::new(b.s.max(1.0), color));
    }
    /// Text at board (x, y) with letter spacing; returns its width in board units.
    #[allow(clippy::too_many_arguments)]
    fn text(&self, x: f32, y: f32, anchor: Align2, s: &str, family: &str, size: f32, color: Color32, spacing: f32) -> f32 {
        let b = self.b;
        self.note(family, s);
        let mut job = egui::text::LayoutJob::default();
        job.append(
            s,
            0.0,
            egui::TextFormat { font_id: b.f(family, size), color, extra_letter_spacing: spacing * b.s, ..Default::default() },
        );
        let galley = self.p.layout_job(job);
        let size = galley.size();
        let rect = anchor.anchor_size(b.p(x, y), size);
        self.p.galley(rect.min, galley, color);
        size.x / b.s
    }
    /// Width of a text in board units.
    fn measure(&self, s: &str, family: &str, size: f32, spacing: f32) -> f32 {
        let b = self.b;
        let mut job = egui::text::LayoutJob::default();
        job.append(s, 0.0, egui::TextFormat { font_id: b.f(family, size), extra_letter_spacing: spacing * b.s, ..Default::default() });
        self.p.layout_job(job).size().x / b.s
    }
    /// Text left-aligned at (x, y), its size reduced (to 60% at most) to fit max_w.
    #[allow(clippy::too_many_arguments)]
    fn text_fit(&self, x: f32, y: f32, s: &str, family: &str, size: f32, max_w: f32, color: Color32, spacing: f32) -> f32 {
        let mut z = size;
        while z > size * 0.6 && self.measure(s, family, z, spacing) > max_w {
            z -= 1.0;
        }
        self.text(x, y, Align2::LEFT_CENTER, s, family, z, color, spacing)
    }
    /// A section label in lavender, optionally with a rule to `rule_to`.
    fn label(&self, x: f32, y: f32, s: &str, rule_to: Option<f32>) {
        let w = self.text(x, y, Align2::LEFT_CENTER, s, "tech", 16.0, LAV, 2.6);
        if let Some(x1) = rule_to {
            self.hline(x + w + 14.0, x1, y, LAVLINE);
        }
    }
}


fn key(p: &Painter, r: Rect, on: bool, hover: bool, disabled: bool) {
    let s = p.b.s;
    p.rect(Rect::from_min_max(r.left_bottom() - vec2(0.0, 1.0), r.right_bottom() + vec2(0.0, 3.0 * s)), hex(0x0d2a2b));
    let (mut top, mut bot) = (KEY_TOP, KEY_BOT);
    if hover {
        top = top.lerp_to_gamma(Color32::WHITE, 0.08);
        bot = bot.lerp_to_gamma(Color32::WHITE, 0.08);
    }
    if disabled {
        top = top.lerp_to_gamma(BG, 0.55);
        bot = bot.lerp_to_gamma(BG, 0.55);
    }
    p.vgrad(r, top, bot);
    if on {
        p.p.rect_stroke(r.expand(3.0 * s), CornerRadius::same(3), Stroke::new(3.0 * s, MINT.gamma_multiply(0.18)), StrokeKind::Outside);
        p.p.rect_stroke(r, CornerRadius::same(2), Stroke::new(2.0 * s, hex(0x8ff0c8)), StrokeKind::Inside);
    } else {
        p.p.rect_stroke(r, CornerRadius::same(2), Stroke::new(2.0 * s, KEYLINE), StrokeKind::Inside);
    }
    p.p.line_segment([r.left_top() + vec2(2.0 * s, 2.0 * s), r.right_top() + vec2(-2.0 * s, 2.0 * s)], Stroke::new(s, Color32::from_white_alpha(30)));
}

fn led(p: &Painter, c: Pos2, on: bool) {
    let s = p.b.s;
    if on {
        p.p.circle_filled(c, 10.0 * s, hex(0x6fe46f).gamma_multiply(0.18));
        p.p.circle_filled(c, 6.5 * s, hex(0x2e8e33));
        p.p.circle_filled(c, 5.0 * s, hex(0x6fe46f));
        p.p.circle_filled(c - vec2(1.5 * s, 1.5 * s), 2.0 * s, hex(0xe8ffe9));
    } else {
        p.p.circle_filled(c, 6.5 * s, hex(0x0b1515));
        p.p.circle_filled(c, 5.5 * s, hex(0x1e2422));
    }
}

fn checkbox(ui: &egui::Ui, p: &Painter, x: f32, y: f32, text: &str, on: bool, enabled: bool) -> bool {
    let b = p.b;
    let box_r = b.r(x, y - 10.0, 20.0, 20.0);
    p.frame(box_r, hex(0xcfc7bf), 2.0);
    if on {
        let pts = [b.p(x + 4.0, y), b.p(x + 8.5, y + 5.0), b.p(x + 16.0, y - 6.0)];
        p.p.line(pts.to_vec(), Stroke::new(3.0 * b.s, MINT));
    }
    let w = p.text(x + 34.0, y, Align2::LEFT_CENTER, text, "tech", 16.0, INK, 0.4);
    let resp = ui.interact(b.r(x, y - 12.0, 34.0 + w, 24.0), ui.id().with(text), Sense::click());
    enabled && resp.clicked()
}

fn hatch(p: &Painter, r: Rect, strength: f32) {
    p.rect(r, hex(0x2b2725));
    let clip = p.p.with_clip_rect(r);
    let step = 6.0 * p.b.s;
    let mut x = r.left() - r.height();
    while x < r.right() {
        clip.line_segment([pos2(x, r.bottom()), pos2(x + r.height(), r.top())], Stroke::new(2.0 * p.b.s, hex(0x3d3835).gamma_multiply(strength.min(1.0))));
        x += step;
    }
    p.p.rect_stroke(r, CornerRadius::ZERO, Stroke::new(p.b.s, hex(0x4d4643)), StrokeKind::Inside);
}

fn lock(p: &Painter, r: Rect) {
    let c = r.center();
    let u = r.width() / 16.0;
    let body = Rect::from_center_size(c + vec2(0.0, 2.5 * u), vec2(8.0 * u, 6.0 * u));
    p.p.rect_filled(body, CornerRadius::ZERO, hex(0x1d1917));
    p.p.circle_stroke(c + vec2(0.0, -1.0 * u), 2.6 * u, Stroke::new(1.6 * u, hex(0x1d1917)));
}

fn lcd_icon(p: &Painter, x: f32, y: f32, synth: bool) {
    // a dot-matrix keyboard (or an empty frame), 32 x 12 "pixels" of 7.4 board units
    let px = 7.4;
    let dot = |cx: usize, cy: usize| p.rect(p.b.r(x + cx as f32 * px, y + cy as f32 * px, px * 0.8, px * 0.8), LCDINK);
    for cx in 0..32 { dot(cx, 0); dot(cx, 11); }
    for cy in 0..12 { dot(0, cy); dot(31, cy); }
    if !synth {
        for cx in (3..29).step_by(2) { dot(cx, 6); }
        return;
    }
    for cx in (3..12).step_by(2) { dot(cx, 3); }
    for cx in (18..29).step_by(3) { dot(cx, 2); dot(cx, 3); }
    for cx in (2..30).step_by(2) { for cy in 6..10 { dot(cx, cy); } }
}

#[derive(Clone, Copy)]
enum Icon { Backup, Restore, Chip, Boot, Bolt }

fn draw_icon(p: &Painter, icon: Icon, x: f32, y: f32, size: f32, color: Color32) {
    let b = p.b;
    let u = size / 32.0;
    let pt = |a: f32, c: f32| b.p(x + a * u, y + c * u);
    let st = Stroke::new(2.0 * u * b.s, color);
    let line = |pts: &[(f32, f32)]| p.p.add(Shape::line(pts.iter().map(|&(a, c)| pt(a, c)).collect(), st));
    match icon {
        Icon::Backup => {
            line(&[(9.0, 3.0), (19.0, 3.0), (25.0, 9.0), (25.0, 29.0), (9.0, 29.0), (9.0, 3.0)]);
            line(&[(19.0, 3.0), (19.0, 9.0), (25.0, 9.0)]);
            line(&[(3.0, 18.0), (15.0, 18.0)]);
            line(&[(11.0, 14.0), (15.0, 18.0), (11.0, 22.0)]);
        }
        Icon::Restore => {
            line(&[(13.0, 3.0), (23.0, 3.0), (29.0, 9.0), (29.0, 29.0), (13.0, 29.0), (13.0, 3.0)]);
            line(&[(23.0, 3.0), (23.0, 9.0), (29.0, 9.0)]);
            line(&[(19.0, 18.0), (5.0, 18.0)]);
            line(&[(9.0, 14.0), (5.0, 18.0), (9.0, 22.0)]);
        }
        Icon::Chip | Icon::Boot => {
            line(&[(8.0, 8.0), (24.0, 8.0), (24.0, 24.0), (8.0, 24.0), (8.0, 8.0)]);
            for k in [12.0, 16.0, 20.0] {
                line(&[(k, 4.0), (k, 8.0)]);
                line(&[(k, 24.0), (k, 28.0)]);
                line(&[(4.0, k), (8.0, k)]);
                line(&[(24.0, k), (28.0, k)]);
            }
            if let Icon::Boot = icon {
                p.p.circle_filled(pt(23.0, 23.0), 6.0 * u * b.s, KEY_BOT);
                p.p.circle_stroke(pt(23.0, 23.0), 3.2 * u * b.s, st);
            }
        }
        Icon::Bolt => {
            let pts = [(18.0, 2.0), (7.0, 18.0), (15.0, 18.0), (13.0, 30.0), (25.0, 13.0), (17.0, 13.0)];
            p.p.add(Shape::convex_polygon(vec![pt(18.0, 2.0), pt(7.0, 18.0), pt(15.0, 18.0)], color, Stroke::NONE));
            p.p.add(Shape::convex_polygon(vec![pt(15.0, 18.0), pt(13.0, 30.0), pt(25.0, 13.0), pt(17.0, 13.0)], color, Stroke::NONE));
            p.p.add(Shape::convex_polygon(vec![pt(18.0, 2.0), pt(15.0, 18.0), pt(17.0, 13.0)], color, Stroke::NONE));
            let _ = pts;
        }
    }
}

/// A tileable grain texture for the panel.
fn noise_texture(ctx: &egui::Context) -> TextureHandle {
    let n = 160;
    let mut seed: u32 = 0x9E3779B9;
    let pixels: Vec<Color32> = (0..n * n)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            Color32::from_white_alpha((seed % 14) as u8)
        })
        .collect();
    let image = egui::ColorImage::new([n, n], pixels);
    let opts = TextureOptions { wrap_mode: TextureWrapMode::Repeat, ..TextureOptions::NEAREST };
    ctx.load_texture("noise", image, opts)
}
