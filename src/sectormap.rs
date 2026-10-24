//! The flash as 256 sectors of 4 KiB (a 16 x 16 map), each labelled with the
//! region it belongs to, from the unit's own file tables.

use crate::boards::{Board, KEY_MAC, SMK37};
use crate::formats::{BOOT_AREA, SECTOR, app_table};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    Boot,
    Firmware,
    Settings,
    Pairings,
    Calibration,
    Presets,
    Sequences,
    Sounds,
    Mac,
    Free,
}

impl Region {
    pub const LEGEND: [Region; 10] = [
        Region::Boot, Region::Firmware, Region::Settings, Region::Pairings, Region::Calibration,
        Region::Presets, Region::Sequences, Region::Free, Region::Sounds, Region::Mac,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Region::Boot => "BOOT",
            Region::Firmware => "FIRMWARE",
            Region::Settings => "SETTINGS",
            Region::Pairings => "PAIRINGS",
            Region::Calibration => "CALIBRATION",
            Region::Presets => "PRESETS",
            Region::Sequences => "SEQUENCES",
            Region::Sounds => "SOUNDS",
            Region::Mac => "MAC",
            Region::Free => "free",
        }
    }
}

fn region_of(name: &str) -> Option<Region> {
    Some(match name {
        "VM" => Region::Settings,
        "BTIF" => Region::Pairings,
        "USRTRIM" => Region::Calibration,
        "USRFLASH" => Region::Presets,
        "SEQ64" => Region::Sequences,
        "USR" => Region::Sounds,
        _ => return None,
    })
}

/// Stock layouts, for a unit whose firmware table cannot be read.
fn stock(board: &Board) -> &'static [(Region, usize, usize)] {
    match board.key {
        "smk37" => &[
            (Region::Firmware, 0x04000, 0x9D000), (Region::Settings, 0x9D000, 0xC0000),
            (Region::Pairings, 0xC0000, 0xC1000), (Region::Calibration, 0xC1000, 0xC2000),
            (Region::Presets, 0xC2000, 0xEB000), (Region::Sounds, 0xF4000, 0xFE000),
        ],
        _ => &[
            (Region::Firmware, 0x04000, 0x93000), (Region::Settings, 0x93000, 0xE9000),
            (Region::Pairings, 0xE9000, 0xEA000), (Region::Sounds, 0xEA000, 0xFC000),
        ],
    }
}

pub fn sector_map(flash: &[u8], key: u16, board: Option<&Board>) -> Vec<Region> {
    let n = flash.len() / SECTOR;
    let mut out = vec![Region::Free; n];
    let mut mark = |r: Region, start: usize, end: usize| {
        for s in start / SECTOR..end.div_ceil(SECTOR).min(n) {
            out[s] = r;
        }
    };
    let table = app_table(flash, key);
    let files: Vec<_> = table.iter().filter(|e| e.attr == 0x82).collect();
    if !files.is_empty() {
        let mut end = BOOT_AREA + files.iter().map(|e| (e.addr + e.len) as usize).max().unwrap_or(0);
        for e in &table {
            // PRCT: the code area the firmware protects
            if e.name == "PRCT" && e.addr == 0 && (e.len as usize) <= flash.len() {
                end = end.max(e.len as usize);
            }
        }
        mark(Region::Firmware, BOOT_AREA, end);
        for e in table.iter().filter(|e| e.attr == 0x12 || e.attr == 0x92) {
            if let Some(r) = region_of(&e.name) {
                if e.len > 0 && (e.addr + e.len) as usize <= flash.len() {
                    mark(r, e.addr as usize, (e.addr + e.len) as usize);
                }
            }
        }
    } else if let Some(b) = board {
        for &(r, a, e) in stock(b) {
            mark(r, a, e);
        }
    }
    if board.is_some_and(|b| b.key == SMK37.key) {
        mark(Region::Calibration, 0xC1000, 0xC2000); // factory data, whatever the firmware declares
    }
    mark(Region::Boot, 0, BOOT_AREA);
    mark(Region::Mac, KEY_MAC.0, KEY_MAC.1);
    out
}

/// "C2-EA, F4-FD" (sector numbers in hex) for a region, for the legend.
pub fn region_ranges(map: &[Region], region: Region) -> String {
    let mut parts = vec![];
    let mut s = 0;
    while s < map.len() {
        let mut e = s;
        while e + 1 < map.len() && map[e + 1] == map[s] {
            e += 1;
        }
        if map[s] == region {
            parts.push(if s == e { format!("{s:02X}") } else { format!("{s:02X}-{e:02X}") });
        }
        s = e + 1;
    }
    parts.join(", ")
}
