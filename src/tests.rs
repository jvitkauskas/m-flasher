//! Tests against a simulated unit with NOR flash behaviour. The tests with real
//! flash images and M-VAVE packages need FLASHER_TEST_DATA (a folder with
//! smk37-stock.bin, smk37-snoop37.bin, smk37-baudgirl.bin, fm1-stock.bin,
//! smk37-016.fwsc, fm1-015.fwsc, fm1-016.fwsc); they are skipped without it.

use crate::boards::{Analysis, CHIP_KEY, FLASH_SIZE, FM1, SMK37};
use crate::formats::{AppStatus, BOOT_AREA, Package, app_files_status, app_table, boot_table, bootloader_identity,
                     build_boot_area, crc16, enc, stored_bootloader};
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
    static NEXT_SESSION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("mflasher-test-{}-{id}", std::process::id()));
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

/// The FM-1 016 directory, with small synthetic files, so its layout and all
/// three file CRCs are exercised even without the vendor's package.
fn fm1_016_image() -> Vec<u8> {
    let mut flash = vec![0xFF; FLASH_SIZE];
    let mut app = vec![0xFF; 0x1C0];
    let files: [(&str, usize, &[u8]); 3] = [
        ("app.bin", 0x140, b"FM-1_016\0"), ("cfg_tool.bin", 0x160, b"config"), ("DX7.bin", 0x180, &[0x5A; 64]),
    ];
    let mut entries = vec![("app_area_head", 0x83, 0x02000140, app.len(), 0)];
    for &(name, at, data) in &files {
        app[at..at + data.len()].copy_from_slice(data);
        entries.push((name, 0x82, at, data.len(), crc16(data)));
    }
    entries.extend([
        ("VM", 0x12, 0xBA000, 0x20000, 0xFFFF), ("PRCT", 0x92, 0, 0xA9000, 0xFFFF),
        ("SEQ64", 0x92, 0xDA000, 0x12000, 0xFFFF), ("BTIF", 0x92, 0xEC000, 0x1000, 0xFFFF),
    ]);
    for (i, (name, attr, addr, len, dcrc)) in entries.iter().enumerate() {
        let mut e = [0u8; 32];
        e[2..4].copy_from_slice(&dcrc.to_le_bytes());
        e[4..8].copy_from_slice(&(*addr as u32).to_le_bytes());
        e[8..12].copy_from_slice(&(*len as u32).to_le_bytes());
        e[12] = *attr;
        e[16..16 + name.len()].copy_from_slice(name.as_bytes());
        let crc = crc16(&e[2..]);
        e[..2].copy_from_slice(&crc.to_le_bytes());
        app[i * 32..(i + 1) * 32].copy_from_slice(&e);
    }
    for (i, block) in app.chunks(32).enumerate() {
        flash[BOOT_AREA + i * 32..BOOT_AREA + (i + 1) * 32].copy_from_slice(&enc(block, CHIP_KEY ^ (i * 8) as u16));
    }
    flash
}

#[test]
fn fm1_016_identification_map_and_dx7_checksum() {
    let mut flash = fm1_016_image();
    let a = Analysis::new(&flash, Some(FM1.flash_id), CHIP_KEY);
    assert_eq!(a.board, Some(&FM1));
    assert!(a.certain);
    assert_eq!(a.firmware_text, "FM-1 016 (stock)");
    assert_eq!(a.app_status, AppStatus::Ok);
    assert_eq!(Analysis::new(&flash, None, CHIP_KEY).board, Some(&FM1), "backups have no flash chip ID");
    assert!(!Analysis::new(&flash, Some(SMK37.flash_id), CHIP_KEY).certain);
    let map = sector_map(&flash, CHIP_KEY, Some(&FM1));
    assert_eq!(region_ranges(&map, Region::Firmware), "04-A8");
    assert_eq!(region_ranges(&map, Region::Settings), "BA-D9");
    assert_eq!(region_ranges(&map, Region::Sequences), "DA-EB");
    assert_eq!(region_ranges(&map, Region::Pairings), "EC");
    assert_eq!(region_ranges(&map, Region::Sounds), "");
    assert_eq!(region_ranges(&map, Region::Free), "A9-B9, ED-FE");
    flash[BOOT_AREA + 0x180] ^= 1;
    assert_eq!(app_files_status(&flash, CHIP_KEY), AppStatus::BadCrc, "DX7.bin is checked along with app.bin and cfg_tool.bin");
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
fn fm1_016_vendor_package() {
    let pkg = Package::parse(&need!("fm1-016.fwsc")).unwrap();
    assert_eq!(pkg.identity, "FM-1_016");
    assert_eq!(pkg.model(), "FM-1");
    assert_eq!(pkg.flash.len(), 0xA9000);
    assert_eq!(app_table(&pkg.flash, CHIP_KEY).iter().filter(|e| e.attr == 0x82).count(), 3);
    assert_eq!(app_files_status(&pkg.flash, CHIP_KEY), AppStatus::Ok);
    let a = Analysis::new(&pkg.flash, Some(FM1.flash_id), CHIP_KEY);
    assert_eq!(a.board, Some(&FM1));
    assert!(a.certain);
    assert_eq!(a.firmware_text, "FM-1 016 (stock)");
    let table = boot_table(&pkg.flash);
    assert_eq!(table[1].addr, 0x3938);
    let boot = stored_bootloader(&pkg.flash).unwrap();
    assert_eq!(boot.len(), 0x3898);
    assert_eq!(build_boot_area(&pkg.flash, &boot).unwrap(), pkg.flash[..BOOT_AREA]);
}

#[test]
fn fm1_016_upgrade_wipe_and_downgrade() {
    let raw = need!("fm1-016.fwsc");
    let old_raw = need!("fm1-015.fwsc");
    let old = need!("fm1-stock.bin");
    let mut f = own(old.clone(), "fm1-016-upgrade");
    f.id = FM1.flash_id;
    let mut s = session_with(f);
    let p = s.plan_firmware(&raw, false, false).unwrap();
    assert_eq!(p.allowed, vec![(BOOT_AREA, 0xA9000)]);
    assert!(!p.warnings.is_empty(), "the changed settings layout is reported");
    assert_eq!(&p.target[..BOOT_AREA], &old[..BOOT_AREA]);
    assert_eq!(&p.target[0xA9000..], &old[0xA9000..]);
    let with_boot = s.plan_firmware(&raw, true, false).unwrap();
    assert_eq!(with_boot.allowed, vec![(0, 0xA9000)]);
    assert_eq!(with_boot.target[..0xA9000], Package::parse(&raw).unwrap().flash);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.analysis.firmware_text, "FM-1 016 (stock)");
    assert_eq!(s.analysis.app_status, AppStatus::Ok);
    assert_eq!(std::fs::read(s.backup_path.as_ref().unwrap()).unwrap(), old);

    let before_wipe = s.dev.mem.clone();
    let p = s.plan_firmware(&raw, false, true).unwrap();
    assert_eq!(p.allowed, vec![(BOOT_AREA, 0xA9000), (0xBA000, 0xDA000), (0xEC000, 0xED000)]);
    assert!(p.target[0xBA000..0xDA000].iter().all(|&b| b == 0xFF));
    assert!(p.target[0xEC000..0xED000].iter().all(|&b| b == 0xFF));
    assert_eq!(&p.target[0xDA000..0xEC000], &before_wipe[0xDA000..0xEC000], "sequences are kept");
    assert_eq!(&p.target[0xFF000..], &old[0xFF000..], "the unit's own factory data is kept");
    apply(&mut s, &p).unwrap();

    let p = s.plan_firmware(&old_raw, false, false).unwrap();
    assert!(!p.warnings.is_empty());
    apply(&mut s, &p).unwrap();
    assert_eq!(s.analysis.firmware_text, "FM-1 015 (stock)");
    assert_eq!(s.analysis.app_status, AppStatus::Ok);
    assert_eq!(&s.dev.mem[BOOT_AREA..0x93000], &old[BOOT_AREA..0x93000]);

    let smk = session(need!("smk37-stock.bin"));
    assert!(matches!(smk.plan_firmware(&raw, false, false), Err(Error::Refused(_))));
    s.dev.key = 0x1234;
    assert!(matches!(s.plan_firmware(&raw, false, false), Err(Error::Refused(_))));
}

#[test]
fn fm1_016_bootloader_replacement_on_both_layouts() {
    let raw = need!("fm1-016.fwsc");
    let pkg = Package::parse(&raw).unwrap();
    let stock_boot = stored_bootloader(&pkg.flash).unwrap();
    let mut new = pkg.flash.clone();
    new.resize(FLASH_SIZE, 0xFF);
    for original in [need!("fm1-stock.bin"), new] {
        let original_config = &boot_table(&original)[1];
        let at = original_config.addr as usize;
        let len = original_config.len as usize;
        let mut f = own(original.clone(), "fm1-016-bootloader");
        f.id = FM1.flash_id;
        let mut s = session_with(f);
        let p = s.plan_bootloader(None, Some(&raw)).unwrap();
        assert_eq!(p.allowed, vec![(0, BOOT_AREA)]);
        assert_eq!(&p.target[BOOT_AREA..], &original[BOOT_AREA..]);
        assert_eq!(&p.target[0x3938..0x3938 + len], &original[at..at + len]);
        apply(&mut s, &p).unwrap();
        assert_eq!(stored_bootloader(&s.dev.mem).unwrap(), stock_boot);
        let stock = s.dev.mem.clone();

        let p = s.plan_bootloader(None, None).unwrap();
        apply(&mut s, &p).unwrap();
        assert_eq!(bootloader_identity(&stored_bootloader(&s.dev.mem).unwrap()).unwrap().0, "fm1");
        assert_eq!(&s.dev.mem[0x3938..], &stock[0x3938..]);
        let p = s.plan_bootloader(None, Some(&raw)).unwrap();
        apply(&mut s, &p).unwrap();
        assert_eq!(s.dev.mem, stock);
    }
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
