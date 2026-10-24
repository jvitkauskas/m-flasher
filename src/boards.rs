//! The devices this tool knows, and how to tell them apart from a flash image.

use crate::formats::{
    AppStatus, BOOT_AREA, app_files_status, app_read, app_table, boot_table, bootloader_identity, stored_bootloader,
};

pub const FLASH_SIZE: usize = 0x100000;
pub const CHIP_KEY: u16 = 0x980F; // every FM-1 and SMK-37 seen so far
pub const KEY_MAC: (usize, usize) = (0xFF000, 0x100000);

#[derive(Debug, PartialEq)]
pub struct Board {
    pub key: &'static str,
    /// Shown to the user.
    pub name: &'static str,
    /// The board name in our bootloader's identity block.
    pub tag: &'static str,
    /// Package models (identity without version).
    pub models: &'static [&'static str],
    /// JEDEC ID seen on units (a hint only).
    pub flash_id: u32,
    /// Per-unit data, never written from another unit's image.
    pub factory: &'static [(usize, usize)],
}

pub static FM1: Board = Board {
    key: "fm1",
    name: "FM-1",
    tag: "fm1",
    models: &["FM-1"],
    flash_id: 0x856014,
    factory: &[KEY_MAC],
};
pub static SMK37: Board = Board {
    key: "smk37",
    name: "SMK-37 Pro / Elite",
    tag: "smk37",
    models: &["SMK-37 Pro", "SMK-37 Elite"],
    flash_id: 0xEB6014,
    factory: &[(0xC1000, 0xC2000), KEY_MAC], // USRTRIM: factory key and pad calibration
};
pub static BOARDS: [&Board; 2] = [&FM1, &SMK37];

const SMK_TRIM_MARKER: (usize, &[u8]) = (0xC1224, b"trimdat\0");
const GUARD_COPY: (usize, &[u8]) = (0xFF000, b"TRIMCOPY"); // our bootloader's copy of the calibration

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BootKind {
    Stock,
    Open,
    Damaged,
    Unknown,
}

/// What a flash image says about the unit: the model (with the evidence), the
/// bootloader and the installed firmware.
pub struct Analysis {
    pub evidence: Vec<(&'static Board, bool, String)>,
    pub board: Option<&'static Board>,
    pub certain: bool,
    pub bootloader: BootKind,
    pub bootloader_text: String,
    pub app_status: AppStatus,
    pub firmware_text: String,
    /// The firmware's own model name ("SMK-37 Pro", ...), if found.
    pub model_name: Option<String>,
}

impl Analysis {
    pub fn new(flash: &[u8], flash_id: Option<u32>, key: u16) -> Analysis {
        let mut a = Analysis {
            evidence: vec![],
            board: None,
            certain: false,
            bootloader: BootKind::Unknown,
            bootloader_text: String::new(),
            app_status: AppStatus::NotFound,
            firmware_text: String::new(),
            model_name: None,
        };
        a.model(flash, flash_id, key);
        a.boot(flash);
        a.firmware(flash, key);
        a
    }

    fn model(&mut self, flash: &[u8], flash_id: Option<u32>, key: u16) {
        let ev = &mut self.evidence;
        if let Some((tag, _)) = stored_bootloader(flash).and_then(|b| bootloader_identity(&b)) {
            for b in BOARDS {
                if b.tag == tag {
                    ev.push((b, true, format!("the installed bootloader is built for the {}", b.name)));
                }
            }
        }
        let has = |(at, m): (usize, &[u8])| flash.len() >= at + m.len() && &flash[at..at + m.len()] == m;
        if has(SMK_TRIM_MARKER) {
            ev.push((&SMK37, true, "SMK-37 factory calibration found".into()));
        }
        if has(GUARD_COPY) {
            ev.push((&SMK37, true, "copy of the SMK-37 calibration found".into()));
        }
        let table = app_table(flash, key);
        let region = |n: &str| table.iter().find(|e| (e.attr == 0x12 || e.attr == 0x92) && e.name == n).map(|e| e.addr);
        if region("USRTRIM") == Some(0xC1000) {
            ev.push((&SMK37, false, "the firmware has the SMK-37 calibration area".into()));
        } else if ((region("VM") == Some(0x93000) || region("BTIF") == Some(0xE9000)) && region("USR") == Some(0xEA000))
            || (region("VM") == Some(0xBA000) && region("SEQ64") == Some(0xDA000) && region("BTIF") == Some(0xEC000)) {
            // stock and Baud Girl keep BTIF at 0xE9000 on the FM-1 (0xC0000 on the SMK-37)
            // FM-1 016 replaces USR with SEQ64 and moves VM and BTIF.
            ev.push((&FM1, false, "the firmware uses the FM-1 flash layout".into()));
        }
        if let Some(id) = flash_id {
            for b in BOARDS {
                if id == b.flash_id {
                    ev.push((b, false, format!("flash chip 0x{id:06X}")));
                }
            }
        }
        let mut strong: Vec<&'static Board> = ev.iter().filter(|e| e.1).map(|e| e.0).collect();
        let mut weak: Vec<&'static Board> = ev.iter().filter(|e| !e.1).map(|e| e.0).collect();
        strong.dedup_by(|a, b| a.key == b.key);
        strong.sort_by_key(|b| b.key);
        strong.dedup_by(|a, b| a.key == b.key);
        weak.sort_by_key(|b| b.key);
        weak.dedup_by(|a, b| a.key == b.key);
        if strong.len() == 1 {
            self.board = Some(strong[0]);
            self.certain = weak.iter().all(|b| b.key == strong[0].key);
        } else if strong.is_empty() && weak.len() == 1 {
            // two independent signals that agree (layout and flash chip), nothing against
            self.board = Some(weak[0]);
            self.certain = ev.iter().filter(|e| e.0.key == weak[0].key).count() >= 2;
        }
    }

    fn boot(&mut self, flash: &[u8]) {
        match stored_bootloader(flash) {
            None if boot_table(flash).is_empty() => {
                self.bootloader = BootKind::Damaged;
                self.bootloader_text = "damaged or missing".into();
            }
            None => {
                self.bootloader = BootKind::Unknown;
                self.bootloader_text = "unreadable".into();
            }
            Some(b) => match bootloader_identity(&b) {
                Some((board, version)) => {
                    self.bootloader = BootKind::Open;
                    // m-bootloader's versions are 1.0-<commit>; the earlier
                    // open bootloader's were bare commit hashes
                    let name = if version.starts_with(|c: char| c.is_ascii_digit()) && version.contains('.') {
                        "m-bootloader"
                    } else {
                        "earlier open bootloader"
                    };
                    self.bootloader_text = format!("{name} {version} ({board})");
                }
                None => {
                    self.bootloader = BootKind::Stock;
                    self.bootloader_text = "stock (M-VAVE)".into();
                }
            },
        }
    }

    fn firmware(&mut self, flash: &[u8], key: u16) {
        self.app_status = app_files_status(flash, key);
        if self.app_status == AppStatus::NotFound {
            self.firmware_text = "none".into();
            return;
        }
        self.firmware_text = "unknown".into();
        let table = app_table(flash, key);
        let Some(app) = table.iter().find(|e| e.name == "app.bin") else { return };
        // Baud Girl keeps its settings at 0xD9000 on both models; its FM-1 build
        // calls itself FM-1_101, like a stock version
        let baud_girl = table.iter().any(|e| e.name == "VM" && e.addr == 0xD9000);
        if BOOT_AREA + (app.addr + app.len) as usize > flash.len() {
            return;
        }
        let text = app_read(flash, BOOT_AREA, app.addr as usize, app.len as usize, key);
        let mut idents = find_identities(&text);
        idents.sort_by(|a, b| a.1.cmp(&b.1));
        idents.dedup();
        if let Some((m, _)) = idents.last() {
            self.model_name = Some(m.clone());
        }
        let name = if let Some(v) = find_after(&text, b"SLOOP ", |c| c.is_ascii_digit() || c == b'.') {
            let mut n = format!("SLOOP {v}");
            if self.model_name.as_deref().is_some_and(|m| m.starts_with("SMK")) {
                n += " (Snoop37)";
            }
            n
        } else if contains(&text, b"Felucca") {
            "Felucca".into()
        } else if let Some((model, version)) = idents.last() {
            let mut n = format!("{model} {version}");
            if model == "SMK-37+VA" || baud_girl {
                n += " (Baud Girl)";
            } else if version != "900" && version != "000" && idents.len() == 1 {
                n += " (stock)";
            }
            n
        } else {
            "unknown".into()
        };
        self.firmware_text = name;
    }

    pub fn model_text(&self) -> String {
        match self.board {
            Some(b) if self.certain => b.name.to_string(),
            Some(b) => format!("{} (probably)", b.name),
            None => "unknown".into(),
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The run of bytes matching `ok` right after the first `prefix` that has one.
fn find_after(hay: &[u8], prefix: &[u8], ok: impl Fn(u8) -> bool) -> Option<String> {
    let mut i = 0;
    while i + prefix.len() < hay.len() {
        if &hay[i..i + prefix.len()] == prefix {
            let s = i + prefix.len();
            let mut e = s;
            while e < hay.len() && ok(hay[e]) {
                e += 1;
            }
            let v = &hay[s..e];
            if v.first().is_some_and(|c| c.is_ascii_digit()) && v.contains(&b'.') {
                return Some(String::from_utf8_lossy(v).trim_end_matches('.').to_string());
            }
        }
        i += 1;
    }
    None
}

/// ("SMK-37 Pro", "016") for every "MODEL_NNN" identity in the text.
fn find_identities(text: &[u8]) -> Vec<(String, String)> {
    const MODELS: [&[u8]; 4] = [b"FM-1", b"SMK-37 Pro", b"SMK-37 Elite", b"SMK-37+VA"];
    let mut out = vec![];
    for m in MODELS {
        let mut i = 0;
        while i + m.len() + 4 <= text.len() {
            if &text[i..i + m.len()] == m && text[i + m.len()] == b'_' {
                let d = &text[i + m.len() + 1..i + m.len() + 4];
                if d.iter().all(|c| c.is_ascii_digit()) {
                    out.push((String::from_utf8_lossy(m).into(), String::from_utf8_lossy(d).into()));
                }
            }
            i += 1;
        }
    }
    out
}
