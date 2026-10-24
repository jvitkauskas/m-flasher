//! File and flash formats of the JieLi AC79 (WL82) devices: CRCs, the ENC
//! cipher, the boot area and its file table, the app area, BANKCB bootloader
//! containers and M-VAVE's .fwsc firmware packages.
//!
//! Flash layout (FM-1 and SMK-37, 1 MiB):
//!
//! ```text
//! 0x00000  boot area, 16 KiB: flash header (32 bytes) and file table
//!          (32-byte entries from 0x20), the bootloader (uboot.boot) at 0xA0,
//!          isd_config.ini after it (0x38D0, or 0x3938 in FM-1 016); the
//!          header, table and bootloader are ENC with key 0xFFFF
//! 0x04000  app area: its own file table (app.bin, cfg_tool.bin and the region
//!          descriptors VM, BTIF, USR, ...), ENC per 32-byte block with the chip
//!          key XOR (offset in the area >> 2)
//! ...      the regions the app's table declares
//! 0xFF000  key_mac: Bluetooth MAC and the flash efuse page
//! ```
//!
//! All CRCs are CRC16/XMODEM.

use std::collections::BTreeMap;
use std::fmt;

pub const SECTOR: usize = 0x1000;
pub const BOOT_AREA: usize = 0x4000;
pub const BOOT_AT: usize = 0xA0;

#[derive(Debug, Clone, PartialEq)]
pub struct FormatError(pub String);

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(s: impl Into<String>) -> Result<T, FormatError> {
    Err(FormatError(s.into()))
}

pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// CRC32 (IEEE, as zlib), the check our loader computes on the chip.
pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data.iter().fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

/// JieLi's ENC cipher (its own inverse).
pub fn enc(data: &[u8], mut key: u16) -> Vec<u8> {
    data.iter()
        .map(|&b| {
            let o = b ^ key as u8;
            key = (key << 1) ^ if key & 0x8000 != 0 { 0x1021 } else { 0 };
            o
        })
        .collect()
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].iter().map(|&c| c as char).collect()
}

// ---- file tables ----

/// A 32-byte file table entry: head CRC, data CRC, address, length, attribute,
/// index, name.
#[derive(Debug, Clone)]
pub struct Entry {
    pub raw: [u8; 32],
    pub data_crc: u16,
    pub addr: u32,
    pub len: u32,
    pub attr: u8,
    pub name: String,
    pub valid: bool,
}

impl Entry {
    pub fn parse(raw: &[u8]) -> Entry {
        let mut r = [0u8; 32];
        r.copy_from_slice(&raw[..32]);
        Entry {
            raw: r,
            data_crc: u16le(&r, 2),
            addr: u32le(&r, 4),
            len: u32le(&r, 8),
            attr: r[12],
            name: cstr(&r[16..32]),
            valid: crc16(&r[2..]) == u16le(&r, 0),
        }
    }
}

pub fn flash_header_valid(flash: &[u8]) -> bool {
    let h = enc(&flash[..32], 0xFFFF);
    crc16(&h[2..]) == u16le(&h, 0)
}

/// The boot area's file table (uboot.boot, isd_config.ini, app_dir_head,
/// key_mac), or empty if the flash header is damaged.
pub fn boot_table(flash: &[u8]) -> Vec<Entry> {
    if flash.len() < BOOT_AREA || !flash_header_valid(flash) {
        return vec![];
    }
    let mut out = vec![];
    for a in (0x20..BOOT_AT).step_by(0x20) {
        let e = Entry::parse(&enc(&flash[a..a + 32], 0xFFFF));
        if !e.valid {
            break;
        }
        out.push(e);
    }
    out
}

/// n plain bytes at offset off of the app area at base.
pub fn app_read(flash: &[u8], base: usize, off: usize, n: usize, key: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 64);
    let mut a = off - off % 32;
    while a < off + n {
        let start = base + a;
        if start + 32 > flash.len() {
            break;
        }
        out.extend(enc(&flash[start..start + 32], key ^ ((a >> 2) as u16)));
        a += 32;
    }
    let s = off % 32;
    out.into_iter().skip(s).take(n).collect()
}

/// The app area's file table: files (app.bin, cfg_tool.bin; addresses in the
/// area) and region descriptors (VM, BTIF, ...; absolute addresses).
pub fn app_table(flash: &[u8], key: u16) -> Vec<Entry> {
    let mut out = vec![];
    for i in 0..16 {
        let raw = app_read(flash, BOOT_AREA, 32 * i, 32, key);
        if raw.len() < 32 {
            break;
        }
        let e = Entry::parse(&raw);
        if !e.valid {
            break;
        }
        out.push(e);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppStatus {
    Ok,
    BadCrc,
    Incomplete,
    NotFound,
}

impl AppStatus {
    pub fn text(self) -> &'static str {
        match self {
            AppStatus::Ok => "OK",
            AppStatus::BadCrc => "DAMAGED (checksum fails; it may still start)",
            AppStatus::Incomplete => "INCOMPLETE (partly erased: an interrupted write)",
            AppStatus::NotFound => "none installed",
        }
    }
}

/// As the bootloader's menu reports it: a damaged file with erased sectors in
/// it is an interrupted write.
pub fn app_files_status(flash: &[u8], key: u16) -> AppStatus {
    let table = app_table(flash, key);
    let files: Vec<&Entry> = table.iter().filter(|e| e.attr == 0x82).collect();
    if files.is_empty() {
        return AppStatus::NotFound;
    }
    for e in files {
        let (start, end) = (BOOT_AREA + e.addr as usize, BOOT_AREA + e.addr as usize + e.len as usize);
        if end > flash.len() {
            return AppStatus::BadCrc;
        }
        if crc16(&app_read(flash, BOOT_AREA, e.addr as usize, e.len as usize, key)) != e.data_crc {
            let mut s = start - start % SECTOR;
            while s < end {
                if flash[s..s + SECTOR].iter().all(|&b| b == 0xFF) {
                    return AppStatus::Incomplete;
                }
                s += SECTOR;
            }
            return AppStatus::BadCrc;
        }
    }
    AppStatus::Ok
}

// ---- bootloader (BANKCB container) ----

/// The payload of a valid BANKCB container.
pub fn bankcb_check(boot: &[u8]) -> Result<&[u8], FormatError> {
    if boot.len() < 16 {
        return err("bootloader file too short");
    }
    let (bank, size, offset, pcrc, hcrc) =
        (u16le(boot, 0), u16le(boot, 2) as usize, u32le(boot, 8) as usize, u16le(boot, 12), u16le(boot, 14));
    let ok = bank == 1 && crc16(&boot[..14]) == hcrc && offset + size <= boot.len();
    if !ok || crc16(&boot[offset..offset + size]) != pcrc {
        return err("not a valid bootloader container (BANKCB)");
    }
    Ok(&boot[offset..offset + size])
}

/// (board, version) of one of our bootloaders; None for stock or other.
pub fn bootloader_identity(boot: &[u8]) -> Option<(String, String)> {
    let p = bankcb_check(boot).ok()?;
    if p.len() < 0x70 || &p[0x40..0x48] != b"AC79BLID" {
        return None;
    }
    Some((cstr(&p[0x48..0x50]), cstr(&p[0x50..0x70])))
}

/// The uboot.boot container stored in a boot area.
pub fn stored_bootloader(flash: &[u8]) -> Option<Vec<u8>> {
    let t = boot_table(flash);
    let e = t.first()?;
    if e.name != "uboot.boot" || e.addr as usize != BOOT_AT || BOOT_AT + e.len as usize > flash.len() {
        return None;
    }
    let s = &flash[BOOT_AT..BOOT_AT + e.len as usize];
    if s.len() < 16 {
        return None;
    }
    let mut boot = enc(&s[..16], 0xFFFF);
    boot.extend(enc(&s[16..], 0xFFFF));
    (crc16(&boot) == e.data_crc).then_some(boot)
}

/// flash's boot area with its bootloader replaced. Keeps the flash header, the
/// file table and isd_config (the unit's own); the bootloader's entry gets the
/// new length and CRC, the space up to isd_config is filled with 0xFF. If a
/// larger bootloader needs the space, moves the unit's configuration further
/// into the boot area and updates its entry. Stored as JieLi's packager writes
/// it: the 16-byte header and the payload each ENC 0xFFFF on their own.
pub fn build_boot_area(flash: &[u8], boot: &[u8]) -> Result<Vec<u8>, FormatError> {
    bankcb_check(boot)?;
    if flash.len() < BOOT_AREA {
        return err("the boot area is incomplete");
    }
    let mut area = flash[..BOOT_AREA].to_vec();
    let t = boot_table(&area);
    if t.len() < 3 || t[0].name != "uboot.boot" || t[0].addr as usize != BOOT_AT {
        return err("the boot area has no valid file table");
    }
    let (isd_at, isd_len) = (t[1].addr as usize, t[1].len as usize);
    if t[1].name != "isd_config.ini" || isd_at < BOOT_AT || isd_at > BOOT_AREA
        || isd_len == 0 || isd_len > BOOT_AREA - isd_at || t[0].len as usize > isd_at - BOOT_AT {
        return err("isd_config is not safely contained in the boot area");
    }
    if boot.len() > BOOT_AREA - BOOT_AT - isd_len {
        return err(format!("bootloader too large: {} bytes, room for {}", boot.len(), BOOT_AREA - BOOT_AT - isd_len));
    }
    let boot_end = BOOT_AT + boot.len();
    let new_isd_at = if boot_end > isd_at { boot_end.next_multiple_of(4) } else { isd_at };
    if new_isd_at + isd_len > BOOT_AREA {
        return err("no room for the bootloader and the unit's configuration in the boot area");
    }
    if new_isd_at != isd_at {
        if area[isd_at + isd_len..new_isd_at + isd_len].iter().any(|&b| b != 0xFF) {
            return err("moving isd_config would overwrite other boot-area data");
        }
        let config = area[isd_at..isd_at + isd_len].to_vec();
        area[isd_at..isd_at + isd_len].fill(0xFF);
        area[new_isd_at..new_isd_at + isd_len].copy_from_slice(&config);
        let mut e = t[1].raw;
        put32(&mut e, 4, new_isd_at as u32);
        let hc = crc16(&e[2..]);
        put16(&mut e, 0, hc);
        area[0x40..0x60].copy_from_slice(&enc(&e, 0xFFFF));
    }
    let mut e = t[0].raw;
    put16(&mut e, 2, crc16(boot));
    put32(&mut e, 4, BOOT_AT as u32);
    put32(&mut e, 8, boot.len() as u32);
    let hc = crc16(&e[2..]);
    put16(&mut e, 0, hc);
    area[0x20..0x40].copy_from_slice(&enc(&e, 0xFFFF));
    let mut stored = enc(&boot[..16], 0xFFFF);
    stored.extend(enc(&boot[16..], 0xFFFF));
    stored.resize(new_isd_at - BOOT_AT, 0xFF);
    area[BOOT_AT..new_isd_at].copy_from_slice(&stored);
    Ok(area)
}

// ---- .fwsc packages ----

const FWSC_BLOCK: usize = 0x30;
const FWSC_KEEP: usize = 0x2F;

/// An M-VAVE firmware package (.fwsc): a JieLi UFW image with the product
/// identity woven in. The file is cut into 48-byte blocks; byte 47 of block i
/// holds identity character i as (char + i + 1) & 0xFF, 0x7D once the identity
/// has ended. Without those bytes: a 64-byte header (CRC16 of bytes 2..63,
/// CRC16 of the file list, total size, file count) and the file list (80 bytes
/// per file: type, index, data CRC16, offset, size, name at 0x40), both ENC
/// 0xFFFF. flash.bin (type 0) is the flash image from address 0: the boot area
/// as stored in flash, then the app area, already encrypted with the chip key
/// the package was made for.
pub struct Package {
    pub identity: String,
    #[allow(dead_code)] // the other files (ota.bin, USR, ...): not used yet
    pub files: BTreeMap<String, (u16, u16, Vec<u8>)>,
    pub flash: Vec<u8>,
}

impl Package {
    pub fn parse(raw: &[u8]) -> Result<Package, FormatError> {
        let bad = || FormatError("not a firmware package (.fwsc)".into());
        if raw.len() < 0x1000 {
            return Err(bad());
        }
        let mut n = 0;
        while n < 20 || raw[n * FWSC_BLOCK + FWSC_KEEP] == 0x7D {
            n += 1;
            if (n + 1) * FWSC_BLOCK > raw.len() {
                return Err(bad());
            }
        }
        let mut identity = String::new();
        for i in 0..n {
            let m = raw[i * FWSC_BLOCK + FWSC_KEEP];
            if m == 0x7D {
                break;
            }
            identity.push(m.wrapping_sub(i as u8 + 1) as char);
        }
        let mut image = Vec::with_capacity(raw.len());
        for i in 0..n {
            image.extend_from_slice(&raw[i * FWSC_BLOCK..i * FWSC_BLOCK + FWSC_KEEP]);
        }
        image.extend_from_slice(&raw[n * FWSC_BLOCK..]);
        let head = enc(&image[..0x40], 0xFFFF);
        if crc16(&head[2..]) != u16le(&head, 0) {
            return err("not a firmware package, or a damaged one (header CRC)");
        }
        let count = u16le(&head, 8) as usize;
        if 0x40 + 0x50 * count > image.len() {
            return err("damaged firmware package (file list)");
        }
        let list = &image[0x40..0x40 + 0x50 * count];
        if crc16(list) != u16le(&head, 2) {
            return err("damaged firmware package (file list CRC)");
        }
        let mut files = BTreeMap::new();
        for i in 0..count {
            let e = enc(&list[i * 0x50..(i + 1) * 0x50], 0xFFFF);
            let (typ, dcrc, off, size) = (u16le(&e, 0), u16le(&e, 4), u32le(&e, 8) as usize, u32le(&e, 12) as usize);
            if off + size > image.len() {
                return err("damaged firmware package (file outside the image)");
            }
            files.insert(cstr(&e[0x40..]), (typ, dcrc, image[off..off + size].to_vec()));
        }
        let flash = match files.get("flash.bin") {
            Some((0, dcrc, data)) => {
                if crc16(data) != *dcrc {
                    return err("damaged firmware package (flash.bin CRC)");
                }
                data.clone()
            }
            _ => return err("the package has no flash image (flash.bin)"),
        };
        if flash.len() <= BOOT_AREA || flash.len() % SECTOR != 0 {
            return err(format!("unexpected flash image size {}", flash.len()));
        }
        Ok(Package { identity, files, flash })
    }

    /// "FM-1", "SMK-37 Pro", ... (the identity without its version).
    pub fn model(&self) -> &str {
        self.identity.rsplit_once('_').map(|(m, _)| m).unwrap_or(&self.identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_enc() {
        assert_eq!(crc16(b"123456789"), 0x31C3); // CRC16/XMODEM check value
        let x = enc(b"hello world", 0xFFFF);
        assert_eq!(enc(&x, 0xFFFF), b"hello world");
    }

    fn boot_container(len: usize) -> Vec<u8> {
        let mut boot = vec![0xA5; len];
        put16(&mut boot, 0, 1);
        put16(&mut boot, 2, (len - 16) as u16);
        put32(&mut boot, 8, 16);
        let crc = crc16(&boot[16..]);
        put16(&mut boot, 12, crc);
        let crc = crc16(&boot[..14]);
        put16(&mut boot, 14, crc);
        boot
    }

    fn boot_entry(name: &str, addr: usize, len: usize, crc: u16) -> Vec<u8> {
        let mut e = [0u8; 32];
        put16(&mut e, 2, crc);
        put32(&mut e, 4, addr as u32);
        put32(&mut e, 8, len as u32);
        e[16..16 + name.len()].copy_from_slice(name.as_bytes());
        let crc = crc16(&e[2..]);
        put16(&mut e, 0, crc);
        enc(&e, 0xFFFF)
    }

    fn boot_image(boot: &[u8], isd_at: usize) -> Vec<u8> {
        let mut flash = vec![0xFF; BOOT_AREA];
        let mut header = [0u8; 32];
        let crc = crc16(&header[2..]);
        put16(&mut header, 0, crc);
        flash[..32].copy_from_slice(&enc(&header, 0xFFFF));
        flash[0x20..0x40].copy_from_slice(&boot_entry("uboot.boot", BOOT_AT, boot.len(), crc16(boot)));
        let config: Vec<u8> = (0..0x2BB).map(|i| i as u8).collect();
        flash[0x40..0x60].copy_from_slice(&boot_entry("isd_config.ini", isd_at, config.len(), crc16(&config)));
        flash[0x60..0x80].copy_from_slice(&boot_entry("app_dir_head", BOOT_AREA, 0, 0));
        flash[BOOT_AT..BOOT_AT + 16].copy_from_slice(&enc(&boot[..16], 0xFFFF));
        flash[BOOT_AT + 16..BOOT_AT + boot.len()].copy_from_slice(&enc(&boot[16..], 0xFFFF));
        flash[isd_at..isd_at + config.len()].copy_from_slice(&config);
        flash
    }

    #[test]
    fn boot_area_uses_the_config_address_in_the_table() {
        for (boot_len, isd_at) in [(0x3830, 0x38D0), (0x3898, 0x3938), (0x3899, 0x3939)] {
            let boot = boot_container(boot_len);
            let flash = boot_image(&boot, isd_at);
            assert_eq!(build_boot_area(&flash, &boot).unwrap(), flash);
            let smaller = boot_container(0x3300);
            let replaced = build_boot_area(&flash, &smaller).unwrap();
            assert_eq!(stored_bootloader(&replaced).unwrap(), smaller);
            assert_eq!(&replaced[isd_at..], &flash[isd_at..]);
            assert_eq!(boot_table(&replaced)[1].addr as usize, isd_at);
        }
    }

    #[test]
    fn larger_bootloader_moves_and_preserves_the_units_config() {
        let flash = boot_image(&boot_container(0x3830), 0x38D0);
        let boot = boot_container(0x3898);
        let replaced = build_boot_area(&flash, &boot).unwrap();
        let config = &boot_table(&replaced)[1];
        assert_eq!(config.addr, 0x3938);
        assert_eq!(config.data_crc, boot_table(&flash)[1].data_crc);
        assert_eq!(&replaced[0x3938..0x3938 + config.len as usize], &flash[0x38D0..0x38D0 + config.len as usize]);
        assert_eq!(stored_bootloader(&replaced).unwrap(), boot);
        assert_eq!(&replaced[..0x20], &flash[..0x20]);
        assert_eq!(&replaced[0x60..BOOT_AT], &flash[0x60..BOOT_AT]);
        assert_eq!(&replaced[0x3938 + config.len as usize..], &flash[0x3938 + config.len as usize..]);
    }

    #[test]
    fn boot_area_refuses_unsafe_config_locations() {
        let boot = boot_container(0x3830);
        for (at, len) in [(BOOT_AT - 1, 1), (0x38CF, 0x2BB), (0x38D0, 0), (BOOT_AREA, 1), (0xFFFF_FFFF, 0xFFFF_FFFF)] {
            let mut flash = boot_image(&boot, 0x38D0);
            flash[0x40..0x60].copy_from_slice(&boot_entry("isd_config.ini", at, len, 0));
            assert!(build_boot_area(&flash, &boot).is_err(), "config at 0x{at:X}, length 0x{len:X}");
        }
        assert!(build_boot_area(&[], &boot).is_err());
    }

    #[test]
    fn bootloader_and_config_must_fit_without_overwriting_other_data() {
        let mut flash = boot_image(&boot_container(0x3830), 0x38D0);
        assert!(build_boot_area(&flash, &boot_container(BOOT_AREA - BOOT_AT)).is_err());
        flash[0x3B90] = 0;
        assert!(build_boot_area(&flash, &boot_container(0x3898)).is_err());
    }
}
