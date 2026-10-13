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
