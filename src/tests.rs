//! Tests against a simulated unit with NOR flash behaviour. The tests with real
//! flash images and M-VAVE packages need FLASHER_TEST_DATA (a folder with
//! smk37-stock.bin, smk37-snoop37.bin, smk37-baudgirl.bin, fm1-stock.bin,
//! smk37-016.fwsc, fm1-015.fwsc, fm1-016.fwsc, minifm-016.fwsc); they are
//! skipped without it.

use crate::boards::{Analysis, CHIP_KEY, FLASH_SIZE, FM1, SMK37};
use crate::formats::{AppStatus, BOOT_AREA, BOOT_AT, CONFIG_AT, CONFIG_KEY, CONFIG_SIZE, Package, app_files_status, app_table, boot_table, bootloader_identity,
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
        assert!(BOOT_AT + boot.len() <= CONFIG_AT);
    }
}

/// The FM-1 016 directory, with small synthetic files, so its layout and all
/// three file CRCs are exercised even without the vendor's package.
fn fm1_016_image() -> Vec<u8> {
    image_016(b"FM-1_016\0")
}

/// The same directory, with the firmware calling itself `identity`.
fn image_016(identity: &[u8]) -> Vec<u8> {
    let mut flash = vec![0xFF; FLASH_SIZE];
    let mut app = vec![0xFF; 0x1C0];
    let files: [(&str, usize, &[u8]); 3] = [
        ("app.bin", 0x140, identity), ("cfg_tool.bin", 0x160, b"config"), ("DX7.bin", 0x180, &[0x5A; 64]),
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

/// A .fwsc with `flash` as its flash.bin and `identity` woven in, as
/// `Package::parse` documents the format.
fn package(identity: &str, flash: &[u8]) -> Vec<u8> {
    const LIST_END: usize = 0x40 + 0x50;
    let mut entry = [0u8; 0x50];
    entry[4..6].copy_from_slice(&crc16(flash).to_le_bytes());
    entry[8..12].copy_from_slice(&(LIST_END as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&(flash.len() as u32).to_le_bytes());
    entry[0x40..0x49].copy_from_slice(b"flash.bin");
    let list = enc(&entry, 0xFFFF);
    let mut head = [0u8; 0x40];
    head[2..4].copy_from_slice(&crc16(&list).to_le_bytes());
    head[8..10].copy_from_slice(&1u16.to_le_bytes());
    let crc = crc16(&head[2..]);
    head[..2].copy_from_slice(&crc.to_le_bytes());
    let mut image = enc(&head, 0xFFFF);
    image.extend_from_slice(&list);
    image.extend_from_slice(flash);
    let mut raw = vec![];
    for i in 0..20 {
        raw.extend_from_slice(&image[i * 47..(i + 1) * 47]);
        raw.push(identity.as_bytes().get(i).map_or(0x7D, |c| c + i as u8 + 1));
    }
    raw.extend_from_slice(&image[20 * 47..]);
    raw
}

/// A session on a unit holding `flash`, with the FM-1's flash chip.
fn fm1_session(flash: Vec<u8>, name: &str) -> Session<FakeFlash> {
    let mut f = own(flash, name);
    f.id = FM1.flash_id;
    session_with(f)
}

#[test]
fn minifm_is_identified_as_an_fm1() {
    let flash = image_016(b"MiniFM_016\0");
    let a = Analysis::new(&flash, Some(FM1.flash_id), CHIP_KEY);
    assert_eq!(a.board, Some(&FM1));
    assert!(a.certain);
    assert_eq!(a.model_name.as_deref(), Some("MiniFM"));
    assert_eq!(a.firmware_text, "MiniFM 016 (stock)");
    assert_eq!(a.app_status, AppStatus::Ok);
}

#[test]
fn minifm_and_fm1_cross_flash_both_ways() {
    let fm1 = fm1_016_image();
    let mini = image_016(b"MiniFM_016\0");
    let fm1_pkg = package("FM-1_016", &fm1[..0xA9000]);
    let mini_pkg = package("MiniFM_016", &mini[..0xA9000]);

    let mut s = fm1_session(mini.clone(), "minifm-to-fm1");
    let p = s.plan_firmware(&fm1_pkg, false, false).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("report itself")), "{:?}", p.warnings);
    assert_eq!(p.warnings.len(), 1, "the layouts are the same: {:?}", p.warnings);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.analysis.firmware_text, "FM-1 016 (stock)");
    assert_eq!(s.analysis.app_status, AppStatus::Ok);
    assert_eq!(std::fs::read(s.backup_path.as_ref().unwrap()).unwrap(), mini, "the MiniFM firmware can be restored");

    let p = s.plan_firmware(&mini_pkg, false, false).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("report itself")), "{:?}", p.warnings);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.analysis.firmware_text, "MiniFM 016 (stock)");
    assert_eq!(s.analysis.app_status, AppStatus::Ok);

    // the model's own package: nothing to warn about
    assert!(s.plan_firmware(&mini_pkg, false, false).unwrap().warnings.is_empty());
    assert!(fm1_session(fm1, "fm1-to-fm1").plan_firmware(&fm1_pkg, false, false).unwrap().warnings.is_empty());
}

#[test]
fn minifm_package_is_refused_on_an_smk37() {
    let mut s = fm1_session(image_016(b"MiniFM_016\0"), "minifm-on-smk37");
    s.board_override = Some(&SMK37);
    let mini_pkg = package("MiniFM_016", &image_016(b"MiniFM_016\0")[..0xA9000]);
    assert!(matches!(s.plan_firmware(&mini_pkg, false, false), Err(Error::Refused(_))));
}

#[test]
fn minifm_016_vendor_package() {
    let raw = need!("minifm-016.fwsc");
    let fm1_raw = need!("fm1-016.fwsc");
    let pkg = Package::parse(&raw).unwrap();
    assert_eq!(pkg.identity, "MiniFM_016");
    assert_eq!(pkg.model(), "MiniFM");
    assert_eq!(pkg.flash.len(), 0xA9000);
    assert_eq!(app_files_status(&pkg.flash, CHIP_KEY), AppStatus::Ok);
    let fm1 = Package::parse(&fm1_raw).unwrap();
    assert_eq!(pkg.flash[..BOOT_AREA], fm1.flash[..BOOT_AREA], "the same boot area");
    let mut padded = pkg.flash.clone();
    padded.resize(FLASH_SIZE, 0xFF);
    let a = Analysis::new(&padded, Some(FM1.flash_id), CHIP_KEY);
    assert_eq!(a.board, Some(&FM1));
    assert!(a.certain);
    assert_eq!(a.firmware_text, "MiniFM 016 (stock)");

    let mut s = fm1_session(padded.clone(), "minifm-016-vendor");
    let p = s.plan_firmware(&fm1_raw, false, false).unwrap();
    assert_eq!(p.warnings.len(), 1, "only the changed name: {:?}", p.warnings);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.analysis.firmware_text, "FM-1 016 (stock)");
    assert_eq!(s.analysis.app_status, AppStatus::Ok);
    let p = s.plan_firmware(&raw, false, false).unwrap();
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.mem, padded, "back to the MiniFM firmware, byte for byte");
}

/// A flash whose boot area holds `boot`.
fn with_boot_area(mut flash: Vec<u8>, boot: &[u8]) -> Vec<u8> {
    let mut head = [0u8; 32];
    let crc = crc16(&head[2..]);
    head[..2].copy_from_slice(&crc.to_le_bytes());
    flash[..32].copy_from_slice(&enc(&head, 0xFFFF));
    let table: [(&str, u32, u32); 3] = [("uboot.boot", BOOT_AT as u32, 16), ("isd_config.ini", 0x3938, 64), ("app_dir_head", 0x4000, 0)];
    for (i, (name, addr, len)) in table.iter().enumerate() {
        let mut e = [0u8; 32];
        e[4..8].copy_from_slice(&addr.to_le_bytes());
        e[8..12].copy_from_slice(&len.to_le_bytes());
        e[16..16 + name.len()].copy_from_slice(name.as_bytes());
        let crc = crc16(&e[2..]);
        e[..2].copy_from_slice(&crc.to_le_bytes());
        flash[0x20 + i * 32..0x40 + i * 32].copy_from_slice(&enc(&e, 0xFFFF));
    }
    // a good chip-key record, then the rest of the configuration
    let key: Vec<u8> = (0..32u8).collect();
    flash[0x3938..0x3958].copy_from_slice(&key);
    flash[0x3958..0x395A].copy_from_slice(&crc16(&key).to_le_bytes());
    let area = build_boot_area(&flash, boot).unwrap();
    flash[..BOOT_AREA].copy_from_slice(&area);
    flash
}

#[test]
fn the_newer_fm1_flash_chip_identifies_the_model() {
    let a = Analysis::new(&fm1_016_image(), Some(0xB36014), CHIP_KEY);
    assert_eq!(a.board, Some(&FM1));
    assert!(a.certain, "the layout and the chip agree");
    assert!(!Analysis::new(&fm1_016_image(), Some(0xEF4014), CHIP_KEY).certain);
}

#[test]
fn installing_m_bootloader_warns_about_an_untested_flash_chip() {
    let (boot, _) = bundled_bootloader(&FM1).unwrap();
    let flash = with_boot_area(fm1_016_image(), boot);
    let with_chip = |id: u32, name: &str| {
        let mut f = own(flash.clone(), name);
        f.id = id;
        session_with(f)
    };
    let untested = with_chip(0xB36014, "untested-chip");
    let p = untested.plan_bootloader(None, None).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("0xB36014")), "{:?}", p.warnings);
    let p = untested.plan_bootloader(Some(boot), None).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("0xB36014")), "a given file is no safer: {:?}", p.warnings);
    // the stock bootloader is what the unit came with
    let pkg = package("FM-1_016", &flash[..0xA9000]);
    assert!(untested.plan_bootloader(None, Some(&pkg)).unwrap().warnings.is_empty());
    for id in [FM1.flash_id, SMK37.flash_id] {
        assert!(with_chip(id, "tested-chip").plan_bootloader(None, None).unwrap().warnings.is_empty());
    }
}

/// `boot` with its version text replaced, its checksums made good again.
fn with_version(boot: &[u8], version: &str) -> Vec<u8> {
    let mut b = boot.to_vec();
    let offset = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let size = u16::from_le_bytes(b[2..4].try_into().unwrap()) as usize;
    b[offset + 0x50..offset + 0x70].fill(0);
    b[offset + 0x50..offset + 0x50 + version.len()].copy_from_slice(version.as_bytes());
    let crc = crc16(&b[offset..offset + size]);
    b[12..14].copy_from_slice(&crc.to_le_bytes());
    let crc = crc16(&b[..14]);
    b[14..16].copy_from_slice(&crc.to_le_bytes());
    b
}

#[test]
fn an_older_bootloader_warns_on_fm1_016s_configuration() {
    let (boot, version) = bundled_bootloader(&FM1).unwrap();
    assert!(!crate::formats::reads_config(boot), "the bundled build reads no configuration: {version}");
    assert!(!crate::formats::reads_config(&with_version(boot, "2.0-abc1234")), "newer majors read none either");
    assert!(crate::formats::reads_config(&with_version(boot, "1.3-6c321e9")));
    let flash_with = |hz: u32| {
        let mut flash = with_boot_area(fm1_016_image(), boot);
        flash[0x3938 + 40..0x3938 + 48].copy_from_slice(b"SYS_CLK\0");
        flash[0x3938 + 48..0x3938 + 52].copy_from_slice(&hz.to_le_bytes());
        flash
    };
    assert_eq!(crate::formats::config_sys_clk(&flash_with(320_000_000)), Some(320_000_000));
    let warned = |hz: u32, b: Option<&[u8]>| {
        let mut f = own(flash_with(hz), "boot-config");
        f.id = FM1.flash_id;
        session_with(f).plan_bootloader(b, None).unwrap().warnings.iter().any(|w| w.contains("BAD CONFIG"))
    };
    let old = with_version(boot, "1.2-22e2559");
    assert!(warned(320_000_000, Some(&old)));
    assert!(warned(320_000_000, Some(&with_version(boot, "abc1234"))), "the earlier bootloader's bare hashes");
    assert!(!warned(360_000_000, Some(&old)), "015's configuration is fine");
    assert!(!warned(320_000_000, Some(&with_version(boot, "1.3-6c321e9"))), "1.3 takes 320 MHz");
    assert!(!warned(320_000_000, Some(&with_version(boot, "1.4-ac26c70"))));
    assert!(!warned(320_000_000, Some(&with_version(boot, "2.0-abc1234"))));
    assert!(!warned(320_000_000, Some(boot)));
    assert!(!warned(320_000_000, None), "the bundled build");
}

/// An FM-1 session on `flash`.
fn fm1_on(flash: Vec<u8>) -> Session<FakeFlash> {
    fm1_session(flash, "short-config")
}

#[test]
fn installing_m_bootloader_cuts_the_configuration_short() {
    let (boot, _) = bundled_bootloader(&FM1).unwrap();
    let old = with_version(boot, "1.3-6c321e9");
    let flash = with_boot_area(fm1_016_image(), &old);
    let key = flash[0x3938..0x3938 + CONFIG_KEY].to_vec();
    let mut s = fm1_on(flash.clone());
    let p = s.plan_bootloader(None, None).unwrap();
    assert_eq!(p.allowed, vec![(0, BOOT_AREA)]);
    assert_eq!(&p.target[BOOT_AREA..], &flash[BOOT_AREA..], "nothing else");
    apply(&mut s, &p).unwrap();
    assert_eq!(stored_bootloader(&s.dev.mem).unwrap(), boot);
    let config = &boot_table(&s.dev.mem)[1];
    assert_eq!((config.addr as usize, config.len as usize), (CONFIG_AT, CONFIG_SIZE));
    assert_eq!(&s.dev.mem[CONFIG_AT..CONFIG_AT + CONFIG_KEY], &key[..]);
    assert_eq!(crate::formats::config_sys_clk(&s.dev.mem), Some(360_000_000));
    // the same again writes nothing
    assert!(s.plan_bootloader(None, None).unwrap().sectors.is_empty());
    // an older bootloader over it: its settings are there, so no warning
    for v in ["1.3-6c321e9", "1.2-22e2559"] {
        let p = s.plan_bootloader(Some(&with_version(boot, v)), None).unwrap();
        assert!(!p.warnings.iter().any(|w| w.contains("BAD CONFIG")), "{v}: {:?}", p.warnings);
        assert_eq!(boot_table(&p.target)[1].addr as usize, CONFIG_AT, "it can only keep what is there");
    }
    // 2.0's shorter configuration has no settings: warned
    let mut shorter = s.dev.mem.clone();
    shorter[CONFIG_AT + CONFIG_KEY..BOOT_AREA].fill(0xFF);
    let p = fm1_on(shorter).plan_bootloader(Some(&old), None).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("BAD CONFIG")), "{:?}", p.warnings);
    // a damaged chip key: refused, and nothing is planned
    let mut damaged = flash.clone();
    damaged[0x3938] ^= 1;
    assert!(matches!(fm1_on(damaged).plan_bootloader(None, None), Err(Error::Refused(_))));
}

#[test]
fn the_stock_bootloader_comes_with_the_packages_whole_boot_area() {
    let (boot, _) = bundled_bootloader(&FM1).unwrap();
    // a stock bootloader: no identity; a package with its own full configuration
    let stock = with_version(boot, "x");
    let stock: Vec<u8> = {
        let mut b = stock;
        let offset = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        let size = u16::from_le_bytes(b[2..4].try_into().unwrap()) as usize;
        b[offset + 0x40..offset + 0x48].fill(0);
        let crc = crc16(&b[offset..offset + size]);
        b[12..14].copy_from_slice(&crc.to_le_bytes());
        let crc = crc16(&b[..14]);
        b[14..16].copy_from_slice(&crc.to_le_bytes());
        b
    };
    assert!(bootloader_identity(&stock).is_none());
    let mut with_config = with_boot_area(fm1_016_image(), &stock);
    with_config[0x3938 + 40..0x3938 + 48].copy_from_slice(b"SYS_CLK\0");
    with_config[0x3938 + 48..0x3938 + 52].copy_from_slice(&360_000_000u32.to_le_bytes());
    let pkg = package("FM-1_016", &with_config[..0xA9000]);

    let mut s = fm1_on(with_config.clone());
    let p = s.plan_bootloader(None, None).unwrap();
    apply(&mut s, &p).unwrap();
    assert!(crate::formats::config_cut_short(&s.dev.mem));
    let p = s.plan_bootloader(None, Some(&pkg)).unwrap();
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    apply(&mut s, &p).unwrap();
    assert_eq!(s.dev.mem[..BOOT_AREA], with_config[..BOOT_AREA], "the whole configuration is back");
    assert_eq!(crate::formats::config_sys_clk(&s.dev.mem), Some(360_000_000));
    // a package for another chip key: its boot area isn't written
    s.dev.key = 0x1234;
    let r = s.plan_bootloader(None, Some(&pkg));
    assert!(matches!(&r, Err(Error::Refused(m)) if m.contains("chip key")), "{:?}", r.err());
    s.dev.key = 0x980F;
    // a stock bootloader from a file, over a short configuration: warned
    let p = s.plan_bootloader(None, None).unwrap();
    apply(&mut s, &p).unwrap();
    let p = s.plan_bootloader(Some(&stock), None).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("cut short")), "{:?}", p.warnings);
}

#[test]
fn the_flash_chip_is_logged_before_the_read() {
    let mut log = vec![];
    Session::open(FakeFlash::new(fm1_016_image()), std::env::temp_dir(), &mut |m| log.push(m), &mut |_, _| {}).ok().unwrap();
    let chip = log.iter().position(|l| l == "Flash chip 0xEB6014").expect("logged");
    assert!(chip < log.iter().position(|l| l == "Reading the flash").unwrap(), "{log:?}");
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
        let mut f = own(original.clone(), "fm1-016-bootloader");
        f.id = FM1.flash_id;
        let mut s = session_with(f);
        let p = s.plan_bootloader(None, Some(&raw)).unwrap();
        assert_eq!(p.allowed, vec![(0, BOOT_AREA)]);
        assert_eq!(&p.target[BOOT_AREA..], &original[BOOT_AREA..]);
        assert_eq!(p.target[..BOOT_AREA], pkg.flash[..BOOT_AREA], "the package's whole boot area");
        apply(&mut s, &p).unwrap();
        assert_eq!(stored_bootloader(&s.dev.mem).unwrap(), stock_boot);
        let stock = s.dev.mem.clone();

        let p = s.plan_bootloader(None, None).unwrap();
        apply(&mut s, &p).unwrap();
        assert_eq!(bootloader_identity(&stored_bootloader(&s.dev.mem).unwrap()).unwrap().0, "fm1");
        assert_eq!(&s.dev.mem[BOOT_AREA..], &stock[BOOT_AREA..]);
        assert_eq!(boot_table(&s.dev.mem)[1].addr as usize, CONFIG_AT);
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
    assert_eq!(s.dev.mem[BOOT_AREA..], stock[BOOT_AREA..]);
    assert_eq!(boot_table(&s.dev.mem)[1].addr as usize, CONFIG_AT);
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
    /// Behave as the ROM and our loader do: without a loader the loader's
    /// commands are not answered, and a jump starts one.
    pub real_loader: bool,
    /// This many loader starts find no flash.
    pub no_flash_starts: usize,
    /// The running loader found no flash: FD05 is refused as out of range.
    pub no_flash: bool,
    /// What FC46 answers, then clears.
    pub last_error: u8,
    /// What the flash chip answers now (FC0A).
    pub flash_id: u32,
    /// The program never returns to the ROM: USB is gone before FB08's answer.
    pub lose_jump: bool,
    /// This many FB06 commands fail.
    pub fail_uploads: usize,
}

/// The version in the bundled loader.
fn bundled_version() -> &'static str {
    let img: &'static [u8] = include_bytes!("../data/mflr-loader-a.bin");
    let at = img.windows(4).position(|w| w == b"MFLR").unwrap();
    std::str::from_utf8(&img[at + 4..at + 7]).unwrap()
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
                if self.fail_uploads > 0 {
                    self.fail_uploads -= 1;
                    return Err(ScsiError::Failed("transfer failed".into()));
                }
                Ok(d.len())
            }
            Data::None => unreachable!(),
            Data::In(b) => {
                self.log.push((cdb.to_vec(), vec![]));
                if op == 0xFB08 && self.lose_jump {
                    return Err(ScsiError::Failed("No such device".into()));
                }
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
                if self.real_loader {
                    if self.loader.is_none() && op != 0xFB08 {
                        return Ok(b.len()); // no echo: the ROM does not know the command
                    }
                    match op {
                        0xFB08 => {
                            let at = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                            self.loader = Some((bundled_version(), at));
                            self.no_flash = self.no_flash_starts > 0;
                            self.no_flash_starts = self.no_flash_starts.saturating_sub(1);
                        }
                        0xFD05 if self.no_flash => {
                            self.last_error = 3;
                            return Ok(16);
                        }
                        0xFD05 => return Ok(b.len()),
                        0xFC0A => {
                            b[2] = 3;
                            b[4..8].copy_from_slice(&self.flash_id.to_le_bytes());
                        }
                        0xFC46 => {
                            b[2] = self.last_error;
                            self.last_error = 0;
                        }
                        _ => {}
                    }
                }
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

/// `Connection::start` on a mock unit: the log and the connection, or the error.
fn start(rom: MockRom) -> (Vec<String>, Result<Connection<MockRom>, ScsiError>) {
    let mut log = vec![];
    let flash_id = if rom.flash_id == 0 { 0xB36014 } else { rom.flash_id };
    let c = Connection::start("/dev/fake", MockRom { real_loader: true, flash_id, ..rom }, &mut |m| log.push(m));
    (log, c)
}

fn jumps(c: &Connection<MockRom>) -> Vec<u32> {
    c.device().log.iter().filter(|(cdb, _)| cdb[..2] == [0xFB, 0x08])
        .map(|(cdb, _)| u32::from_be_bytes(cdb[2..6].try_into().unwrap())).collect()
}

const SLOT_A: u32 = 0x01C2_0000;
const SLOT_B: u32 = 0x01C2_8000;

#[test]
fn a_loader_that_has_the_flash_is_kept() {
    let (log, c) = start(MockRom::default());
    let c = c.ok().unwrap();
    assert_eq!(jumps(&c), vec![SLOT_A], "started once: {log:?}");
    assert_eq!(c.flash_id(), 0xB36014);

    let (log, c) = start(MockRom { loader: Some((bundled_version(), SLOT_B)), ..Default::default() });
    assert!(jumps(&c.ok().unwrap()).is_empty(), "reused: {log:?}");
    assert!(log[0].contains("already running"), "{log:?}");
}

#[test]
fn a_running_loader_without_flash_is_started_again_in_the_other_slot() {
    for (at, other) in [(SLOT_A, SLOT_B), (SLOT_B, SLOT_A)] {
        let (log, c) = start(MockRom { loader: Some((bundled_version(), at)), no_flash: true, ..Default::default() });
        let mut c = c.ok().unwrap();
        assert_eq!(jumps(&c), vec![other], "{log:?}");
        assert!(log.iter().any(|l| l.contains("found no flash chip")), "{log:?}");
        assert_eq!(c.flash_id(), 0xB36014);
        assert_eq!(c.read(0, 32, &mut |_| {}).unwrap().len(), 32);
    }
}

#[test]
fn a_new_loader_without_flash_is_started_once_more() {
    let (log, c) = start(MockRom { no_flash_starts: 1, ..Default::default() });
    assert_eq!(jumps(&c.ok().unwrap()), vec![SLOT_A, SLOT_B], "{log:?}");
}

#[test]
fn a_flash_that_never_answers_is_a_clear_error() {
    let (log, c) = start(MockRom { no_flash_starts: 9, flash_id: 0xFFFFFF, ..Default::default() });
    let Err(ScsiError::Failed(e)) = c else { panic!("expected an error: {log:?}") };
    assert!(e.contains("0xFFFFFF") && e.contains("Nothing was written"), "{e}");
    assert_eq!(log.iter().filter(|l| l.starts_with("Starting")).count(), 2, "not tried for ever: {log:?}");
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
fn run_ram_program_that_never_returns_is_not_a_failure() {
    let job = RamJob::new(&[0xAA; 600], Some("0x01C28000"), &[], None, None).unwrap();
    let mut c = ram_unit(MockRom { lose_jump: true, ..Default::default() });
    let mut log = vec![];
    job.run(&mut c, &mut |s| log.push(s)).unwrap();
    let ops: Vec<[u8; 2]> = sent(&c).iter().map(|(cdb, _)| [cdb[0], cdb[1]]).collect();
    assert_eq!(ops, vec![[0xFB, 6], [0xFB, 6], [0xFB, 8]], "the jump goes once");
    assert!(log.iter().any(|l| l.contains("can't be confirmed")), "{log:?}");
    assert!(!log.iter().any(|l| l.contains("still running")), "{log:?}");
}

#[test]
fn run_ram_calls_need_the_jump_answered() {
    let job = RamJob::new(&[0xAA; 16], Some("0x01C28000"), &["FC60".into()], None, None).unwrap();
    let mut c = ram_unit(MockRom { lose_jump: true, ..Default::default() });
    assert!(job.run(&mut c, &mut |_| {}).is_err());
    assert!(!sent(&c).iter().any(|(cdb, _)| cdb[0] == 0xFC), "the call is not sent");
}

#[test]
fn run_ram_upload_errors_are_still_errors() {
    let mut c = ram_unit(MockRom { fail_uploads: usize::MAX, ..Default::default() });
    assert!(c.run_ram(0x01C2_8000, &[1; 16]).is_err());
    assert!(!sent(&c).iter().any(|(cdb, _)| cdb[..2] == [0xFB, 0x08]), "no jump after a failed upload");
}

#[test]
fn run_ram_upload_sends_a_failed_block_again() {
    let mut c = ram_unit(MockRom { fail_uploads: 1, ..Default::default() });
    c.run_ram(0x01C2_8000, &[1; 16]).unwrap();
    let ops: Vec<[u8; 2]> = sent(&c).iter().map(|(cdb, _)| [cdb[0], cdb[1]]).collect();
    assert_eq!(ops, vec![[0xFB, 6], [0xFB, 6], [0xFB, 8]]);
    assert_eq!(sent(&c)[0], sent(&c)[1]);
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
    assert!(check_ram_range(0x01C2_0000, 0x5FD80).is_ok(), "exactly fills it");
    assert!(check_ram_range(0x01C2_0000, 0x5FD81).is_err());
    assert!(check_ram_range(0x01C3_0000, 0x4FD80).is_ok(), "exactly fills the app slot");
    assert!(check_ram_range(0x01C3_0000, 0x4FD81).is_err());
    assert!(check_ram_range(0x01C7_FD7F, 1).is_ok());
    assert!(check_ram_range(0x01C7_FD7F, 2).is_err());
    assert!(check_ram_range(0x01C7_FD80, 1).is_err(), "the retained area is outside");
    assert!(check_ram_range(0x01C1_FFFF, 1).is_err());
    assert!(check_ram_range(0, 1).is_err());
    assert!(check_ram_range(0xFFFF_FFFF, 1).is_err());
    assert!(check_ram_range(0x01C2_0000, 0).is_err(), "empty image");
    // run_ram refuses before sending anything
    let mut c = ram_unit(MockRom::default());
    assert!(c.run_ram(0x01C1_0000, &[1; 16]).is_err());
    assert!(c.run_ram(0x01C7_FF00, &[1; 16]).is_err());
    assert!(c.run_ram(0x01C7_FC00, &[1; 0x181]).is_err());
    assert!(c.device().log.is_empty());
    // the command line says so before it connects
    let job = |addr: Option<&str>, len: usize| RamJob::new(&vec![0; len], addr, &[], None, None).map(|_| ());
    assert!(job(Some("0x01C28000"), 100).is_ok());
    assert!(job(Some("1c28000"), 100).is_ok());
    assert!(job(Some("0x01C30000"), 0x4FD80).is_ok());
    assert!(matches!(job(Some("0x01C30000"), 0x4FD81), Err(Error::Refused(_))));
    assert!(matches!(job(Some("0x01C7FD80"), 1), Err(Error::Refused(_))));
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

#[test]
fn restart_uploads_the_reset_program_and_jumps_to_it() {
    let mut c = ram_unit(MockRom::default());
    assert_eq!(c.restart().unwrap(), 0x01C2_8000);
    let cmds = sent(&c);
    assert_eq!(cmds.len(), 2, "one block and the jump");
    assert_eq!(crc_cipher(&cmds[0].1, 0xFFFF_FFFF), crate::jl::RESTART);
    assert_eq!(cmds[1].0, cdb16(&[0xFB, 0x08, 0x01, 0xC2, 0x80, 0x00, 0x00, 0x01]));
    // the same upload as run-ram's
    let mut r = ram_unit(MockRom::default());
    r.run_ram(0x01C2_8000, &crate::jl::RESTART).unwrap();
    assert_eq!(sent(&r), cmds);
}

#[test]
fn restart_avoids_the_running_loader() {
    let mut c = ram_unit(MockRom { loader: Some(("1.0", 0x01C2_8000)), ..Default::default() });
    assert_eq!(c.restart().unwrap(), 0x01C2_0000);
    let mut c = ram_unit(MockRom { loader: Some(("1.0", 0x01C2_0000)), ..Default::default() });
    assert_eq!(c.restart().unwrap(), 0x01C2_8000);
}

#[test]
fn asks_for_the_sg_driver_only_when_a_unit_needs_it() {
    use crate::scsi::sg_missing_for_unit;
    let root = std::env::temp_dir().join(format!("mflasher-sg-{}", std::process::id()));
    let (class, usb) = (root.join("scsi_generic"), root.join("usb"));
    let device = |name: &str, vendor: &str, product: &str| {
        let d = usb.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("idVendor"), format!("{vendor}\n")).unwrap();
        std::fs::write(d.join("idProduct"), format!("{product}\n")).unwrap();
    };
    assert!(!sg_missing_for_unit(&class, &usb), "no USB at all");
    device("1-1", "4c4a", "c755");
    device("1-2", "05ac", "8057");
    std::fs::create_dir(usb.join("usb1")).unwrap();
    assert!(!sg_missing_for_unit(&class, &usb), "running firmware and other devices");
    device("1-3", "4c4a", "8057");
    assert!(sg_missing_for_unit(&class, &usb), "the unit in download mode, no driver");
    std::fs::create_dir(&class).unwrap();
    assert!(!sg_missing_for_unit(&class, &usb), "driver loaded");
    std::fs::remove_dir_all(&root).unwrap();
}
