//! Tests against a simulated unit with NOR flash behaviour. The tests with real
//! flash images and M-VAVE packages need FLASHER_TEST_DATA (a folder with
//! smk37-stock.bin, smk37-snoop37.bin, smk37-baudgirl.bin, fm1-stock.bin,
//! smk37-016.fwsc, fm1-015.fwsc); they are skipped without it.

use crate::boards::{Analysis, FM1, SMK37};
use crate::formats::{AppStatus, BOOT_AREA, Package, app_files_status, bootloader_identity, build_boot_area, stored_bootloader};
use crate::jl::Flash;
use crate::ops::{Error, Plan, Session, bundled_bootloader};
use crate::scsi::ScsiError;
use crate::sectormap::{Region, region_ranges, sector_map};

fn data(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var("FLASHER_TEST_DATA").ok()?;
    std::fs::read(std::path::Path::new(&dir).join(name)).ok()
}

macro_rules! need {
    ($name:expr) => {
        match data($name) {
            Some(d) => d,
            None => {
                eprintln!("skipped: {} not in FLASHER_TEST_DATA", $name);
                return;
            }
        }
    };
}

/// NOR flash: erase sets to 0xFF, programming ANDs bits in. fail_writes:
/// corrupt that many programs.
pub struct FakeFlash {
    pub mem: Vec<u8>,
    pub key: u16,
    pub id: u32,
    pub fail_writes: usize,
    pub programmed: usize,
    /// Behave like our loader: on-chip CRC32 and a unique ID.
    pub own: Option<String>,
    pub reads: usize,
}

impl FakeFlash {
    pub fn new(mem: Vec<u8>) -> Self {
        FakeFlash { mem, key: 0x980F, id: 0xEB6014, fail_writes: 0, programmed: 0, own: None, reads: 0 }
    }
}

impl Flash for FakeFlash {
    fn path(&self) -> &str {
        "/dev/fake"
    }
    fn chip_key(&self) -> u16 {
        self.key
    }
    fn flash_id(&self) -> u32 {
        self.id
    }
    fn read(&mut self, addr: usize, n: usize, progress: &mut dyn FnMut(usize)) -> Result<Vec<u8>, ScsiError> {
        self.reads += 1;
        progress(n);
        Ok(self.mem[addr..addr + n].to_vec())
    }
    fn crc32(&mut self, addr: usize, n: usize) -> Result<Option<u32>, ScsiError> {
        Ok(self.own.as_ref().map(|_| crate::formats::crc32(&self.mem[addr..addr + n])))
    }
    fn unique_id(&mut self) -> Option<String> {
        self.own.clone()
    }
    fn erase(&mut self, addr: usize, n: usize) -> Result<(), ScsiError> {
        assert!(addr % 0x1000 == 0 && n % 0x1000 == 0);
        self.mem[addr..addr + n].fill(0xFF);
        Ok(())
    }
    fn program(&mut self, addr: usize, data: &[u8], progress: &mut dyn FnMut(usize)) -> Result<(), ScsiError> {
        self.programmed += 1;
        let mut d = data.to_vec();
        if self.fail_writes > 0 {
            self.fail_writes -= 1;
            let m = d.len() / 2;
            d[m] ^= 1;
        }
        for (i, b) in d.iter().enumerate() {
            self.mem[addr + i] &= b;
        }
        progress(data.len());
        Ok(())
    }
}

fn session(mem: Vec<u8>) -> Session<FakeFlash> {
    session_with(FakeFlash::new(mem))
}

fn session_with(f: FakeFlash) -> Session<FakeFlash> {
    let dir = std::env::temp_dir().join(format!("mflasher-test-{}", std::process::id()));
    Session::open(f, dir, &mut |_| {}, &mut |_, _| {}).ok().unwrap()
}

fn apply(s: &mut Session<FakeFlash>, p: &Plan) -> Result<(), Error> {
    s.apply(p, &mut |_| {}, &mut |_| {})
}

#[test]
fn plan_refuses_changes_outside_allowed() {
    let cur = vec![0xFF; 0x100000];
    let mut tgt = cur.clone();
    tgt[0x5000] = 0;
    assert!(matches!(Plan::new("x".into(), &cur, tgt, vec![(0, 0x4000)], vec![], vec![]), Err(Error::Refused(_))));
}

#[test]
fn plan_runs() {
    let cur = vec![0xFF; 0x100000];
    let mut tgt = cur.clone();
    for a in [0x4000, 0x5000, 0x9000] {
        tgt[a] = 0;
    }
    let p = Plan::new("x".into(), &cur, tgt, vec![(0, 0x100000)], vec![], vec![]).unwrap();
    assert_eq!(p.runs(), vec![(0x4000, 0x6000), (0x9000, 0xA000)]);
}

#[test]
fn bundled_bootloaders() {
    for b in [&FM1, &SMK37] {
        let (boot, _) = bundled_bootloader(b).unwrap();
        assert_eq!(bootloader_identity(boot).unwrap().0, b.tag);
        assert!(0xA0 + boot.len() <= 0x38D0);
    }
}

#[test]
fn reads_must_match() {
    struct Flaky(FakeFlash, usize);
    impl Flash for Flaky {
        fn path(&self) -> &str { "x" }
        fn chip_key(&self) -> u16 { 0x980F }
        fn flash_id(&self) -> u32 { 0 }
        fn read(&mut self, a: usize, n: usize, p: &mut dyn FnMut(usize)) -> Result<Vec<u8>, ScsiError> {
            self.1 += 1;
            let mut v = self.0.read(a, n, p)?;
            if self.1 == 2 { v[100] ^= 1; }
            Ok(v)
        }
        fn erase(&mut self, a: usize, n: usize) -> Result<(), ScsiError> { self.0.erase(a, n) }
        fn program(&mut self, a: usize, d: &[u8], p: &mut dyn FnMut(usize)) -> Result<(), ScsiError> { self.0.program(a, d, p) }
    }
    let r = Session::open(Flaky(FakeFlash::new(vec![0xFF; 0x100000]), 0), std::env::temp_dir(), &mut |_| {}, &mut |_, _| {});
    assert!(matches!(r, Err(Error::Refused(_))));
}

#[test]
fn analysis_identifies_the_known_images() {
    let a = Analysis::new(&need!("smk37-stock.bin"), Some(0xEB6014), 0x980F);
    assert_eq!(a.board.unwrap().key, "smk37");
    assert!(a.certain);
    assert_eq!(a.bootloader_text, "stock (M-VAVE)");
    assert_eq!(a.firmware_text, "SMK-37 Pro 016 (stock)");
    let a = Analysis::new(&need!("fm1-stock.bin"), Some(0x856014), 0x980F);
    assert_eq!(a.board.unwrap().key, "fm1");
    assert!(a.certain);
    assert_eq!(a.firmware_text, "FM-1 015 (stock)");
    let a = Analysis::new(&need!("smk37-snoop37.bin"), None, 0x980F);
    assert_eq!(a.firmware_text, "SLOOP 2.6 (Snoop37)");
    let a = Analysis::new(&need!("smk37-baudgirl.bin"), None, 0x980F);
    assert_eq!(a.firmware_text, "SMK-37+VA 101 (Baud Girl)");
    assert_eq!(a.board.unwrap().key, "smk37");
}

#[test]
fn analysis_identifies_baud_girl_on_the_fm1() {
    // its FM-1 build calls itself FM-1_101 and moves VM to 0xD9000
    let a = Analysis::new(&need!("fm1-baudgirl.bin"), Some(0x856014), 0x980F);
    assert_eq!(a.board.unwrap().key, "fm1");
    assert!(a.certain);
    assert_eq!(a.firmware_text, "FM-1 101 (Baud Girl)");
}

#[test]
fn sector_maps_of_the_known_images() {
    let m = sector_map(&need!("smk37-stock.bin"), 0x980F, Some(&SMK37));
    assert_eq!(region_ranges(&m, Region::Firmware), "04-9C");
    assert_eq!(region_ranges(&m, Region::Free), "EB-F3, FE");
    assert_eq!(region_ranges(&m, Region::Calibration), "C1");
    let m = sector_map(&need!("smk37-baudgirl.bin"), 0x980F, Some(&SMK37));
    assert_eq!(region_ranges(&m, Region::Firmware), "04-BF");
    assert_eq!(region_ranges(&m, Region::Settings), "D9-E8");
    let m = sector_map(&need!("fm1-stock.bin"), 0x980F, Some(&FM1));
    assert_eq!(region_ranges(&m, Region::Settings), "93-E8");
    assert_eq!(region_ranges(&m, Region::Sounds), "EA-FB");
}

#[test]
fn package_boot_area_round_trip() {
    let pkg = Package::parse(&need!("smk37-016.fwsc")).unwrap();
    assert_eq!(pkg.identity, "SMK-37 Pro_016");
    let boot = stored_bootloader(&pkg.flash).unwrap();
    assert_eq!(build_boot_area(&pkg.flash, &boot).unwrap(), pkg.flash[..BOOT_AREA]);
    let fm = Package::parse(&need!("fm1-015.fwsc")).unwrap();
    assert_eq!(fm.model(), "FM-1");
}

#[test]
fn firmware_over_snoop37() {
    let mut s = session(need!("smk37-snoop37.bin"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap();
    assert_eq!(p.sectors.len(), 153);
    assert!(p.sectors.iter().all(|&a| (BOOT_AREA..0x9D000).contains(&a)));
    assert!(p.warnings.is_empty());
    apply(&mut s, &p).unwrap();
    let stock = need!("smk37-stock.bin");
    assert_eq!(s.dev.mem[BOOT_AREA..0x9D000], stock[BOOT_AREA..0x9D000]);
    assert_eq!(app_files_status(&s.dev.mem, 0x980F), AppStatus::Ok);
    assert!(s.backup_path.as_ref().unwrap().exists());
}

#[test]
fn firmware_refusals_and_warnings() {
    let s = session(need!("smk37-stock.bin"));
    assert!(matches!(s.plan_firmware(&need!("fm1-015.fwsc"), false, false), Err(Error::Refused(_))));
    let mut f = FakeFlash::new(need!("smk37-stock.bin"));
    f.key = 0x1234;
    let s = session_with(f);
    assert!(matches!(s.plan_firmware(&need!("smk37-016.fwsc"), false, false), Err(Error::Refused(_))));
    let s = session(need!("smk37-baudgirl.bin"));
    assert!(!s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap().warnings.is_empty());
}

#[test]
fn firmware_wipe_settings() {
    let s = session(need!("smk37-snoop37.bin"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, true).unwrap();
    assert!(p.allowed.contains(&(0x9D000, 0xC0000)));
    assert!(p.allowed.contains(&(0xC0000, 0xC1000)));
    assert!(!p.sectors.iter().any(|&a| (0xC1000..0xC2000).contains(&a)));
}

#[test]
fn open_bootloader_and_back_to_stock() {
    let stock = need!("smk37-stock.bin");
    let mut s = session(stock.clone());
    let p = s.plan_bootloader(None, None).unwrap();
    assert!(!p.sectors.is_empty() && p.sectors.iter().all(|&a| a < BOOT_AREA));
    apply(&mut s, &p).unwrap();
    assert_eq!(bootloader_identity(&stored_bootloader(&s.dev.mem).unwrap()).unwrap().0, "smk37");
    assert_eq!(s.dev.mem[0x38D0..], stock[0x38D0..]);
    let p = s.plan_bootloader(None, Some(&need!("smk37-016.fwsc"))).unwrap();
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.mem, stock);
    let (fm1, _) = bundled_bootloader(&FM1).unwrap();
    assert!(matches!(s.plan_bootloader(Some(fm1), None), Err(Error::Refused(_))));
}

#[test]
fn restore_same_unit_other_unit_other_model() {
    let stock = need!("smk37-stock.bin");
    let mut s = session(need!("smk37-snoop37.bin"));
    let p = s.plan_restore(&stock, None).unwrap();
    assert_eq!(p.allowed, vec![(0, 0x100000)]);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.mem, stock);
    let mut other = stock.clone();
    other[0xC1100] ^= 0xFF;
    other[0xFF800] ^= 0xFF;
    let s = session(stock.clone());
    let p = s.plan_restore(&other, None).unwrap();
    assert!(!p.warnings.is_empty() && p.sectors.is_empty());
    assert!(matches!(s.plan_restore(&need!("fm1-stock.bin"), None), Err(Error::Refused(_))));
}

#[test]
fn write_retried_then_refused() {
    let mut s = session(need!("smk37-snoop37.bin"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap();
    s.dev.fail_writes = 1;
    apply(&mut s, &p).unwrap();
    let mut s = session(need!("smk37-snoop37.bin"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap();
    s.dev.fail_writes = usize::MAX;
    assert!(matches!(apply(&mut s, &p), Err(Error::Refused(_))));
}

fn own(mem: Vec<u8>, id: &str) -> FakeFlash {
    let mut f = FakeFlash::new(mem);
    f.own = Some(id.into());
    f
}

#[test]
fn own_loader_reads_once_and_verifies_by_crc32() {
    let mut s = session_with(own(need!("smk37-snoop37.bin"), "1120050204010521"));
    assert_eq!(s.dev.reads, 1, "one read plus the chip's CRC32");
    assert_eq!(s.unit_id.as_deref(), Some("1120050204010521"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap();
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.reads, 1, "verified by CRC32, not read back");
    let stock = need!("smk37-stock.bin");
    assert_eq!(s.dev.mem[BOOT_AREA..0x9D000], stock[BOOT_AREA..0x9D000]);
    // a corrupted program is caught by the CRC32 and rewritten
    let mut s = session_with(own(need!("smk37-snoop37.bin"), "x"));
    let p = s.plan_firmware(&need!("smk37-016.fwsc"), false, false).unwrap();
    s.dev.fail_writes = 1;
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.mem[BOOT_AREA..0x9D000], stock[BOOT_AREA..0x9D000]);
}

#[test]
fn unit_id_decides_same_unit() {
    let stock = need!("smk37-stock.bin");
    let mut other = stock.clone();
    other[0xC1100] ^= 0xFF;             // factory data differs ...
    let s = session_with(own(stock.clone(), "aaaaaaaaaaaaaaaa"));
    let p = s.plan_restore(&other, Some("aaaaaaaaaaaaaaaa")).unwrap();
    assert_eq!(p.allowed, vec![(0, 0x100000)], "... but the IDs say it is this unit's backup");
    let p = s.plan_restore(&stock, Some("bbbbbbbbbbbbbbbb")).unwrap();
    assert!(!p.warnings.is_empty() && p.allowed != vec![(0, 0x100000)], "identical data, another unit's ID");
}

#[test]
fn backups_in_the_same_second_keep_both() {
    let dir = std::env::temp_dir().join(format!("mflasher-backups-{}", std::process::id()));
    let mut s = session(vec![0xFF; 0x100000]);
    let a = s.save_backup(Some(&dir)).unwrap();
    s.flash[0] = 0;
    let b = s.save_backup(Some(&dir)).unwrap();
    assert_ne!(a, b);
    assert_eq!(std::fs::read(&a).unwrap()[0], 0xFF);
    assert_eq!(std::fs::read(&b).unwrap()[0], 0);
    assert!(a.with_extension("json").exists() && b.with_extension("json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- run-ram ---

use crate::cli::RamJob;
use crate::formats::crc16;
use crate::jl::{Connection, crc_cipher};
use crate::scsi::{Data, Scsi};

/// A unit in download mode with a user program running: records every command
/// (CDB and OUT data) after the inquiry, echoes the opcode of every command
/// with a data-IN phase of 16 bytes, answers FD61 from a RAM that holds
/// `addr as u8 ^ 0x5A` everywhere.
#[derive(Default)]
pub struct MockRom {
    pub log: Vec<(Vec<u8>, Vec<u8>)>,
    /// A running M-Flasher loader: (version, address); answers FC40 as it does.
    pub loader: Option<(&'static str, u32)>,
    /// Fail this many FD61 commands (a transfer error), then work.
    pub fail_reads: usize,
    /// Send one byte less than asked for in FD61.
    pub short_reads: bool,
}

impl Scsi for MockRom {
    fn execute(&mut self, cdb: &[u8], data: Data) -> Result<usize, ScsiError> {
        if cdb[0] == 0x12 {
            if let Data::In(b) = data {
                b[8..32].fill(b' ');
                b[8..12].copy_from_slice(b"WL82");
                b[16..25].copy_from_slice(b"UBOOT1.00");
            }
            return Ok(36);
        }
        let op = u16::from_be_bytes([cdb[0], cdb[1]]);
        match data {
            Data::Out(d) => {
                self.log.push((cdb.to_vec(), d.to_vec()));
                Ok(d.len())
            }
            Data::None => unreachable!(),
            Data::In(b) => {
                self.log.push((cdb.to_vec(), vec![]));
                if op == 0xFD61 {
                    if self.fail_reads > 0 {
                        self.fail_reads -= 1;
                        return Err(ScsiError::Failed("transfer failed".into()));
                    }
                    let a = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                    for (i, x) in b.iter_mut().enumerate() {
                        *x = (a as usize + i) as u8 ^ 0x5A;
                    }
                    return Ok(b.len() - self.short_reads as usize);
                }
                b.fill(0);
                b[..2].copy_from_slice(&cdb[..2]);
                if let (0xFC40, Some((v, at))) = (op, self.loader) {
                    b[2..6].copy_from_slice(b"MFLR");
                    b[6..9].copy_from_slice(v.as_bytes());
                    b[10..14].copy_from_slice(&at.to_be_bytes());
                }
                Ok(b.len())
            }
        }
    }
}

fn ram_unit(rom: MockRom) -> Connection<MockRom> {
    Connection::open("/dev/fake", rom).unwrap()
}

/// The commands that are not the loader probe (FC40).
fn sent(c: &Connection<MockRom>) -> Vec<&(Vec<u8>, Vec<u8>)> {
    c.device().log.iter().filter(|(cdb, _)| cdb[..2] != [0xFC, 0x40]).collect()
}

fn cdb16(head: &[u8]) -> Vec<u8> {
    let mut c = head.to_vec();
    c.resize(16, 0xFF);
    c
}

#[test]
fn run_ram_uploads_like_the_loader_and_jumps() {
    let image: Vec<u8> = (0..1300u32).map(|i| (i * 7 + 3) as u8).collect();
    let mut c = ram_unit(MockRom::default());
    c.run_ram(0x01C2_8000, &image).unwrap();
    let cmds = sent(&c);
    assert_eq!(cmds.len(), 4, "three blocks and the jump");
    for (i, (cdb, out)) in cmds[..3].iter().enumerate() {
        let plain = &image[i * 512..(i * 512 + 512).min(image.len())];
        // scrambled with the ROM's cipher: applying it again gives the image back
        assert_eq!(&crc_cipher(out, 0xFFFF_FFFF), plain);
        assert_ne!(out.as_slice(), plain);
        let mut head = vec![0xFB, 0x06];
        head.extend((0x01C2_8000u32 + 512 * i as u32).to_be_bytes());
        head.extend((plain.len() as u16).to_be_bytes());
        head.push(0);
        head.extend(crc16(out).to_le_bytes()); // over the transmitted (scrambled) block
        assert_eq!(cdb, &cdb16(&head), "block {i}");
    }
    assert_eq!(cmds[2].1.len(), 276);
    // FB08: entry address, target argument 1, a 16-byte answer
    assert_eq!(cmds[3].0, cdb16(&[0xFB, 0x08, 0x01, 0xC2, 0x80, 0x00, 0x00, 0x01]));
}

#[test]
fn run_ram_upload_is_the_loaders_upload() {
    // the loader goes through the same blocks: the bundled loader's first
    // block, sent as the loader, is what run-ram sends for the same bytes
    let (addr, img) = (0x01C2_0000u32, include_bytes!("../data/mflr-loader-a.bin"));
    let mut c = ram_unit(MockRom::default());
    c.run_ram(addr, img).unwrap();
    let blocks: Vec<_> = sent(&c).iter().filter(|(cdb, _)| cdb[..2] == [0xFB, 0x06]).map(|(_, o)| o.clone()).collect();
    let expected: Vec<Vec<u8>> = img.chunks(512).map(|b| crc_cipher(b, 0xFFFF_FFFF)).collect();
    assert_eq!(blocks, expected);
}

#[test]
fn run_ram_address_checks() {
    use crate::jl::check_ram_range;
    assert!(check_ram_range(0x01C2_0000, 1).is_ok());
    assert!(check_ram_range(0x01C2_0000, 0x10000).is_ok(), "exactly fills it");
    assert!(check_ram_range(0x01C2_0000, 0x10001).is_err());
    assert!(check_ram_range(0x01C2_FFFF, 1).is_ok());
    assert!(check_ram_range(0x01C2_FFFF, 2).is_err());
    assert!(check_ram_range(0x01C3_0000, 1).is_err(), "the end is outside");
    assert!(check_ram_range(0x01C1_FFFF, 1).is_err());
    assert!(check_ram_range(0, 1).is_err());
    assert!(check_ram_range(0xFFFF_FFFF, 1).is_err());
    assert!(check_ram_range(0x01C2_0000, 0).is_err(), "empty image");
    // run_ram refuses before sending anything
    let mut c = ram_unit(MockRom::default());
    assert!(c.run_ram(0x01C1_0000, &[1; 16]).is_err());
    assert!(c.run_ram(0x01C2_FF00, &[1; 257]).is_err());
    assert!(c.device().log.is_empty());
    // the command line says so before it connects
    let job = |addr: Option<&str>, len: usize| RamJob::new(&vec![0; len], addr, &[], None, None).map(|_| ());
    assert!(job(Some("0x01C28000"), 100).is_ok());
    assert!(job(Some("1c28000"), 100).is_ok());
    assert!(matches!(job(Some("0x01C30000"), 1), Err(Error::Refused(_))));
    assert!(matches!(job(Some("0x01C2F000"), 0x1001), Err(Error::Refused(_))));
    assert!(matches!(job(Some("0x01C00000"), 1), Err(Error::Refused(_))));
    assert!(matches!(job(Some("zz"), 1), Err(Error::Refused(_))));
    assert!(matches!(job(None, 1), Err(Error::Refused(_))));
}

#[test]
fn run_ram_never_overwrites_the_running_loader() {
    let mut c = ram_unit(MockRom { loader: Some(("1.0", 0x01C2_0000)), ..Default::default() });
    assert!(c.run_ram(0x01C2_0000, &[1; 16]).is_err());
    assert!(c.run_ram(0x01C2_7F00, &[1; 512]).is_err(), "reaches into its slot");
    assert!(sent(&c).is_empty());
    c.run_ram(0x01C2_8000, &[1; 16]).unwrap();
    assert_eq!(sent(&c).len(), 2, "the other slot works while the loader runs");
    let mut c = ram_unit(MockRom { loader: Some(("1.0", 0x01C2_8000)), ..Default::default() });
    assert!(c.run_ram(0x01C2_C000, &[1; 16]).is_err());
    c.run_ram(0x01C2_0000, &[1; 0x8000]).unwrap();
}

#[test]
fn run_ram_call_sends_the_opcode_and_returns_the_answer() {
    let mut c = ram_unit(MockRom::default());
    let r = c.call_program(0xFC60).unwrap();
    assert_eq!(r[..2], [0xFC, 0x60]);
    assert_eq!(sent(&c)[0].0, cdb16(&[0xFC, 0x60]));
    c.call_program(0x0123).unwrap();
    assert_eq!(sent(&c)[1].0, cdb16(&[0x01, 0x23]));
    // flash erase / write, memory write and jump are not sent
    for op in [0xFB00, 0xFB01, 0xFB04, 0xFB06, 0xFB08] {
        assert!(c.call_program(op).is_err());
    }
    assert_eq!(sent(&c).len(), 2);
}

#[test]
fn run_ram_read_is_chunked_and_advances_the_address() {
    let mut c = ram_unit(MockRom::default());
    let mut seen = vec![];
    let data = c.read_ram(0x01C2_0010, 10000, &mut |d| seen.push(d)).unwrap();
    assert_eq!(data.len(), 10000);
    assert!(data.iter().enumerate().all(|(i, &b)| b == (0x01C2_0010usize + i) as u8 ^ 0x5A));
    assert_eq!(seen, vec![4096, 8192, 10000]);
    let cmds: Vec<_> = sent(&c).iter().map(|(cdb, _)| cdb.clone()).collect();
    assert_eq!(cmds, vec![
        cdb16(&[0xFD, 0x61, 0x01, 0xC2, 0x00, 0x10, 0x10, 0x00]),
        cdb16(&[0xFD, 0x61, 0x01, 0xC2, 0x10, 0x10, 0x10, 0x00]),
        cdb16(&[0xFD, 0x61, 0x01, 0xC2, 0x20, 0x10, 0x07, 0x10]),   // 1808 bytes
    ]);
    // exactly one chunk, and a length that is not a multiple of anything
    let mut c = ram_unit(MockRom::default());
    assert_eq!(c.read_ram(0x01C2_0000, 4096, &mut |_| {}).unwrap().len(), 4096);
    assert_eq!(sent(&c).len(), 1);
    assert_eq!(c.read_ram(0x01C2_0000, 1, &mut |_| {}).unwrap(), vec![0x5A]);
}

#[test]
fn run_ram_read_retries_a_failed_transfer_but_not_a_short_one_silently() {
    let mut c = ram_unit(MockRom { fail_reads: 1, ..Default::default() });
    assert_eq!(c.read_ram(0x01C2_0000, 100, &mut |_| {}).unwrap().len(), 100);
    assert_eq!(c.retries().0, 1);
    assert_eq!(sent(&c).len(), 2, "the same command twice");
    assert_eq!(sent(&c)[0], sent(&c)[1]);
    let mut c = ram_unit(MockRom { short_reads: true, ..Default::default() });
    let e = c.read_ram(0x01C2_0000, 100, &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("99 of 100"), "{e}");
}

#[test]
fn run_ram_command_line_job() {
    let dir = std::env::temp_dir().join(format!("mflasher-runram-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("ram.bin");
    let job = RamJob::new(&[0xAA; 600], Some("0x01C28000"), &["FC60".into(), "0xfc61".into()],
                          Some("0x01C28000:5000"), Some(out.to_str().unwrap())).unwrap();
    let mut c = ram_unit(MockRom::default());
    let mut log = vec![];
    job.run(&mut c, &mut |s| log.push(s)).unwrap();
    // 2 upload blocks, the jump, 2 calls, 2 read chunks, in this order
    let ops: Vec<[u8; 2]> = sent(&c).iter().map(|(cdb, _)| [cdb[0], cdb[1]]).collect();
    assert_eq!(ops, vec![[0xFB, 6], [0xFB, 6], [0xFB, 8], [0xFC, 0x60], [0xFC, 0x61], [0xFD, 0x61], [0xFD, 0x61]]);
    assert!(log.iter().any(|l| l.starts_with("0xFC60: fc 60 00")), "{log:?}");
    let got = std::fs::read(&out).unwrap();
    assert_eq!(got.len(), 5000);
    assert_eq!(got[0], (0x01C2_8000u32 as u8) ^ 0x5A);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_ram_command_line_checks() {
    let new = |calls: &[&str], read: Option<&str>, out: Option<&str>| {
        let calls: Vec<String> = calls.iter().map(|s| s.to_string()).collect();
        RamJob::new(&[1; 16], Some("0x01C20000"), &calls, read, out).map(|_| ())
    };
    assert!(new(&[], None, None).is_ok());
    assert!(new(&["FC60"], Some("01C20000:16"), Some("o.bin")).is_ok());
    assert!(new(&[], Some("0x01C20000:0x10"), Some("o.bin")).is_ok());
    assert!(new(&["10000"], None, None).is_err(), "more than 16 bits");
    assert!(new(&["xyz"], None, None).is_err());
    assert!(new(&["FB00"], None, None).is_err());
    assert!(new(&["fb08"], None, None).is_err());
    assert!(new(&[], Some("01C20000:16"), None).is_err(), "--read needs -o");
    assert!(new(&[], None, Some("o.bin")).is_err(), "-o needs --read");
    assert!(new(&[], Some("01C20000"), Some("o")).is_err(), "no length");
    assert!(new(&[], Some("01C20000:0"), Some("o")).is_err());
    assert!(new(&[], Some("01C20000:99999999999"), Some("o")).is_err());
    assert!(new(&[], Some("FFFFFFF0:32"), Some("o")).is_err(), "past the end of the address space");
}
