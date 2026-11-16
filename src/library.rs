//! The firmware library: The Firmware Hub's list of FM-1 firmwares, kept on
//! this computer, and the packages downloaded from their authors' releases.
//!
//! The hub's rules for its list (https://thefirmwarehub.com/kb/#catalog):
//! read it at most once a day with a User-Agent that names the tool, download
//! the packages from the makers' releases and not from the hub, install only a
//! package whose SHA-256 matches the list, and name the hub with its address.
//! The list is CC BY 4.0; each firmware belongs to its author.
//!
//! ```text
//! <data dir>/catalog.json       the list as fetched
//! <data dir>/catalog-fetched    when it was fetched (RFC 3339)
//! <data dir>/packages/ID-VER.fwsc   the packages, each checked when saved
//! <data dir>/packages/index.json     which firmware and version each package is
//! <data dir>/stock/ID.fwsc           M-VAVE's official packages, imported
//! <data dir>/favorites.json          the firmwares starred, by id
//! ```

use crate::boards::FM1;
use crate::formats::Package;
use crate::jl::Flash;
use crate::ops::{Error, Plan, Session};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const CATALOG_URL: &str = "https://thefirmwarehub.com/catalog.json";
pub const ATTRIBUTION: &str = "Firmware list: The Firmware Hub – https://thefirmwarehub.com (CC BY 4.0)";
/// The hub asks for one read a day at most.
const CATALOG_MAX_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(24);
const USAGE: &str = "usage: m-flasher library sync | list [--chip old|new] [--kind KIND] [--beta] | get ID | import-stock DIR |
                         install ID [--skip-bootloader | --stock-bootloader] [--yes] [--restart] [--device PATH]
       sync          fetch The Firmware Hub's list (at most once a day)
       list          the FM-1 firmwares in it, and the official ones imported; --chip new: marks those for the
                     older flash chip only
       get           download a firmware from its author's release and check its SHA-256
       import-stock  copy M-VAVE's official packages (FM-1 015, 016, MiniFM 016) from a folder, known by SHA-256
       install       install a firmware of the library on the unit in download mode. A unit with M-VAVE's stock
                     bootloader gets m-bootloader first (--skip-bootloader: not); --stock-bootloader puts back
                     M-VAVE's bootloader from an official package first";

/// M-VAVE's official packages, known by SHA-256: (SHA-256, id, version, name).
pub const OFFICIAL: [(&str, &str, &str, &str); 3] = [
    ("db1642b2b6fa5c2cccb11ffd13878068bb28601678d3644049f99dc40e7edb8a", "mvave-015", "015", "M-VAVE FM-1 015 (official)"),
    ("d017fb65e70aca97ee29d944470c3515a74581df5c0576bf70de917c7b2b61d5", "mvave-016", "016", "M-VAVE FM-1 016 (official)"),
    ("f10b297b3f0e1789110ba1832b12865b7fc39d5d6e0e7c9a375461339268ca77", "fame-minifm-016", "016", "Fame MiniFM 016 (official)"),
];

pub fn official(sha: &str) -> Option<(&'static str, &'static str, &'static str)> {
    OFFICIAL.iter().find(|o| o.0 == sha).map(|o| (o.1, o.2, o.3))
}

/// "old" (Puya, 0x856014) or "new" (UCUN, 0xB36014) for an FM-1's flash chip.
pub fn unit_chip(flash_id: u32) -> Option<&'static str> {
    if flash_id == FM1.flash_id {
        Some("old")
    } else if FM1.other_flash.contains(&flash_id) {
        Some("new")
    } else {
        None
    }
}

/// A package ready to install: checked against the list or known as official.
pub struct Installable {
    pub id: String,
    pub name: String,
    pub version: String,
    pub chip: String,
    pub data: Vec<u8>,
    /// One of M-VAVE's own packages: its boot area may put back the stock bootloader.
    pub official: bool,
    pub credit: String,
    /// Where the package is kept.
    pub path: PathBuf,
}

impl Installable {
    pub fn fits_chip(&self, unit: Option<&str>) -> bool {
        !(unit == Some("new") && self.chip == "old")
    }
}

/// How an install treats the unit's bootloader (REQ-BOOT-01).
#[derive(Default)]
pub struct InstallOptions {
    /// Leave M-VAVE's stock bootloader as it is.
    pub skip_bootloader: bool,
    /// Put back M-VAVE's bootloader from the (official) package first.
    pub stock_bootloader: bool,
}

/// The step before the firmware, if any (REQ-BOOT-01): m-bootloader on a unit
/// that has M-VAVE's stock bootloader, a known model and a tested flash chip;
/// or, when asked, M-VAVE's own bootloader from the official package.
pub fn bootloader_step<F: Flash>(s: &Session<F>, inst: &Installable, o: &InstallOptions) -> Result<Option<(Plan, String)>, Error> {
    if o.stock_bootloader {
        if !inst.official {
            return Err(Error::Refused(format!("{} is not an official M-VAVE package: the stock bootloader is put back only \
                                               from one (import it with `library import-stock`)", inst.name)));
        }
        let p = s.plan_bootloader(None, Some(&inst.data))?;
        return Ok(Some((p, "M-VAVE's bootloader goes back first, from the official package's boot area.".into())));
    }
    if o.skip_bootloader {
        return Ok(None);
    }
    let Some(key) = s.bootloader_suggestion() else { return Ok(None) };
    let p = s.plan_bootloader(None, None)?;
    Ok(Some((p, format!("This unit has M-VAVE's stock bootloader, which cannot be reflashed from its keys. m-bootloader \
                         goes first: if the new firmware does not start, hold REC while switching on for download mode, \
                         or {key} for the recovery menu. (--skip-bootloader leaves it out.)"))))
}

/// After an install: the recommendation, if the unit still has the stock bootloader.
pub fn after_install_note<F: Flash>(s: &Session<F>, o: &InstallOptions) -> Option<String> {
    (!o.stock_bootloader && s.bootloader_suggestion().is_some()).then(|| {
        "This unit still has M-VAVE's stock bootloader: if a firmware ever fails to start, only a USB_KEY dongle reaches \
         download mode. `m-flasher bootloader` installs m-bootloader.".into()
    })
}

/// Where downloads come from: the network, or a test's table.
pub trait Fetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

pub struct Http(ureq::Agent);

impl Http {
    pub fn new() -> Http {
        let ua = format!("m-flasher-library/{} (+https://github.com/jvitkauskas/m-flasher fork)", env!("CARGO_PKG_VERSION"));
        Http(ureq::AgentBuilder::new().user_agent(&ua).timeout(std::time::Duration::from_secs(60)).build())
    }
}

impl Fetch for Http {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let resp = self.0.get(url).set("Accept", "application/vnd.github+json, application/json, */*").call()
            .map_err(|e| format!("{url}: {e}"))?;
        let mut out = vec![];
        resp.into_reader().take(64 << 20).read_to_end(&mut out).map_err(|e| format!("{url}: {e}"))?;
        Ok(out)
    }
}

/// One firmware of the list.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    /// GitHub owner/name, where the package is released.
    pub repo: Option<String>,
    pub tag: Option<String>,
    /// The release's page.
    pub release: Option<String>,
    /// A pattern for the package's file name among the release's assets.
    pub asset_pkg: Option<String>,
    /// The package's path on the hub: its file name is a hint for the asset's.
    pub pkg: String,
    pub sha256: String,
    pub size: u64,
    /// "old": only for the FM-1's older flash chip; "new"; "any".
    pub chip: String,
    pub license: String,
    pub kinds: Vec<String>,
    pub beta: bool,
    /// Not installed from a package (the hub links to the author's own installer).
    pub elsewhere: bool,
    /// M-VAVE's own firmware.
    pub stock: bool,
    /// Where the stock package is published (a GitHub file page).
    pub source: Option<String>,
}

impl Entry {
    pub fn file_name(&self) -> String {
        let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect::<String>();
        format!("{}-{}.fwsc", safe(&self.id), safe(&self.version))
    }

    /// Whether the unit's flash chip ("old", "new") can take it.
    pub fn fits_chip(&self, unit: Option<&str>) -> bool {
        !(unit == Some("new") && self.chip == "old")
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string).filter(|x| !x.is_empty())
}

pub struct Catalog {
    pub entries: Vec<Entry>,
    pub updated: String,
}

impl Catalog {
    pub fn parse(raw: &[u8]) -> Result<Catalog, String> {
        let v: Value = serde_json::from_slice(raw).map_err(|e| format!("the firmware list is not valid JSON: {e}"))?;
        let list = v.get("firmwares").and_then(Value::as_array).ok_or("the firmware list has no \"firmwares\"")?;
        let mut entries = vec![];
        if let Some(st) = v.get("stock") {
            if let (Some(pkg), Some(sha)) = (s(st, "pkg"), s(st, "sha256")) {
                let product = s(st, "product").unwrap_or_else(|| "FM-1".into());
                entries.push(Entry {
                    id: format!("mvave-{}", product.rsplit('_').next().unwrap_or("stock").to_lowercase()),
                    name: "M-VAVE FM-1 (official)".into(),
                    version: product.rsplit('_').next().unwrap_or("").into(),
                    author: "M-VAVE".into(),
                    repo: None, tag: None, release: None, asset_pkg: None,
                    pkg, sha256: sha.to_lowercase(),
                    size: st.get("size").and_then(Value::as_u64).unwrap_or(0),
                    chip: "any".into(),
                    license: "Apache-2.0".into(),
                    kinds: vec!["stock".into()],
                    beta: false, elsewhere: false, stock: true,
                    source: s(st, "source"),
                });
            }
        }
        for f in list {
            let (Some(id), Some(pkg), Some(sha)) = (s(f, "id"), s(f, "pkg"), s(f, "sha256")) else { continue };
            let kinds = match f.get("kind") {
                Some(Value::String(k)) => vec![k.clone()],
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                _ => vec![],
            };
            let flag = |k: &str| f.get(k).and_then(Value::as_bool).unwrap_or(false);
            entries.push(Entry {
                name: s(f, "name").unwrap_or_else(|| id.clone()),
                version: s(f, "version").unwrap_or_default(),
                author: s(f, "author").unwrap_or_default(),
                repo: s(f, "repo"), tag: s(f, "tag"), release: s(f, "release"), asset_pkg: s(f, "assetPkg"),
                pkg, sha256: sha.to_lowercase(),
                size: f.get("size").and_then(Value::as_u64).unwrap_or(0),
                chip: s(f, "chip").unwrap_or_else(|| "any".into()),
                license: s(f, "lic").unwrap_or_else(|| "?".into()),
                kinds,
                beta: flag("beta"),
                elsewhere: flag("linkOnly") || flag("externalInstall"),
                stock: false,
                source: s(f, "source"),
                id,
            });
        }
        Ok(Catalog { entries, updated: s(&v, "updated").unwrap_or_default() })
    }

    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id.eq_ignore_ascii_case(id))
    }
}

/// Whether a firmware matches a search: every word typed, in any of its
/// texts (name, id, author, kind, version), ignoring case.
pub fn matches_search(query: &str, texts: &[&str]) -> bool {
    let hay = texts.join(" ").to_lowercase();
    query.to_lowercase().split_whitespace().all(|w| hay.contains(w))
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The library's folder: MFLASHER_LIBRARY_DIR, else the system's place for an
/// app's data.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MFLASHER_LIBRARY_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    // under sudo (install talks to the unit as root) the library is still the user's own
    #[cfg(all(unix, not(target_os = "android")))]
    let home = crate::cli::sudo_user_home().unwrap_or(home);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/M-Flasher/library")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or(home).join("M-Flasher").join("library")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("m-flasher/library")
    }
}

/// Writes through a temporary file, so that a file is either whole or absent.
fn write_whole(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub struct Library {
    pub dir: PathBuf,
}

/// What `sync` did.
#[derive(Debug, PartialEq)]
pub enum Synced {
    Fetched,
    /// The list on disk is less than a day old: not fetched again.
    Recent(String),
}

impl Library {
    pub fn new(dir: PathBuf) -> Library {
        Library { dir }
    }

    fn catalog_path(&self) -> PathBuf {
        self.dir.join("catalog.json")
    }

    fn fetched_path(&self) -> PathBuf {
        self.dir.join("catalog-fetched")
    }

    pub fn package_path(&self, e: &Entry) -> PathBuf {
        self.dir.join("packages").join(e.file_name())
    }

    /// The list on disk, if any.
    pub fn catalog(&self) -> Result<Option<Catalog>, String> {
        match std::fs::read(self.catalog_path()) {
            Ok(raw) => Catalog::parse(&raw).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", self.catalog_path().display())),
        }
    }

    /// When the list on disk was fetched.
    pub fn fetched_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let t = std::fs::read_to_string(self.fetched_path()).ok()?;
        chrono::DateTime::parse_from_rfc3339(t.trim()).ok().map(|d| d.with_timezone(&chrono::Utc))
    }

    /// Fetches the list, unless the one on disk is less than a day old.
    pub fn sync(&self, net: &dyn Fetch, now: chrono::DateTime<chrono::Utc>) -> Result<Synced, String> {
        if let Some(at) = self.fetched_at() {
            if now - at < CATALOG_MAX_AGE && self.catalog_path().exists() {
                return Ok(Synced::Recent(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
            }
        }
        let raw = net.get(CATALOG_URL)?;
        let cat = Catalog::parse(&raw)?;
        if cat.entries.is_empty() {
            return Err("the firmware list came back empty: kept the one on disk".into());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        write_whole(&self.catalog_path(), &raw)?;
        write_whole(&self.fetched_path(), now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true).as_bytes())?;
        Ok(Synced::Fetched)
    }

    /// Whether the package is on disk and still matches the list.
    pub fn downloaded(&self, e: &Entry) -> bool {
        std::fs::read(self.package_path(e)).is_ok_and(|d| sha256_hex(&d) == e.sha256)
    }

    /// Packages on disk that the list no longer names.
    pub fn orphans(&self, cat: &Catalog) -> Vec<PathBuf> {
        let names: Vec<String> = cat.entries.iter().map(Entry::file_name).collect();
        std::fs::read_dir(self.dir.join("packages")).map(|rd| {
            rd.filter_map(Result::ok).map(|d| d.path())
                .filter(|p| p.extension().is_some_and(|x| x == "fwsc"))
                .filter(|p| !names.iter().any(|n| p.file_name().is_some_and(|f| f == n.as_str())))
                .collect()
        }).unwrap_or_default()
    }

    /// Where the package may be, in the order to try: the author's GitHub
    /// release (M-VAVE's own: its GitHub file), or with no release the files in
    /// the author's repository; never the hub. Named as the list says first,
    /// then FM-1 builds ("fm1" in the name), then the other .fwsc: the SHA-256
    /// decides which one is the package (a release may hold one per model).
    pub fn resolve(&self, e: &Entry, net: &dyn Fetch) -> Result<Vec<String>, String> {
        if e.stock {
            let src = e.source.as_deref().ok_or("the list gives no source for M-VAVE's package")?;
            return src.strip_prefix("https://github.com/").and_then(|rest| {
                let (repo, path) = rest.split_once("/blob/")?;
                Some(vec![format!("https://raw.githubusercontent.com/{repo}/{path}")])
            }).ok_or_else(|| format!("unexpected source for M-VAVE's package: {src}"));
        }
        let where_ = e.release.clone().or(e.source.clone()).unwrap_or_default();
        if e.elsewhere {
            return Err(format!("{} installs with its author's own installer: {where_}", e.name));
        }
        let Some(repo) = &e.repo else {
            return Err(format!("{} has no GitHub release in the list to download from: {where_}", e.name));
        };
        let json = |raw: Vec<u8>| serde_json::from_slice::<Value>(&raw).map_err(|err| format!("GitHub's answer for {repo}: {err}"));
        // (file name, URL) of each .fwsc
        let files: Vec<(String, String)> = match &e.tag {
            Some(tag) => {
                let rel = json(net.get(&format!("https://api.github.com/repos/{repo}/releases/tags/{tag}"))?)?;
                rel.get("assets").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| {
                    Some((s(x, "name")?, s(x, "browser_download_url")?))
                }).filter(|(n, _)| n.ends_with(".fwsc")).collect()).unwrap_or_default()
            }
            None => {
                // no release: the package as a file in the author's repository (zp12, Tonitor, CS-1…)
                let found = (|| -> Result<Vec<(String, String)>, String> {
                    let info = json(net.get(&format!("https://api.github.com/repos/{repo}"))?)?;
                    let branch = s(&info, "default_branch").unwrap_or_else(|| "main".into());
                    let tree = json(net.get(&format!("https://api.github.com/repos/{repo}/git/trees/{branch}?recursive=1"))?)?;
                    Ok(tree.get("tree").and_then(Value::as_array).map(|t| t.iter()
                        .filter(|x| x.get("type").and_then(Value::as_str) == Some("blob"))
                        .filter_map(|x| s(x, "path")).filter(|p| p.ends_with(".fwsc"))
                        .map(|p| (p.rsplit('/').next().unwrap_or(&p).to_string(), format!("https://raw.githubusercontent.com/{repo}/{branch}/{p}")))
                        .collect()).unwrap_or_default())
                })();
                match found {
                    Ok(f) if !f.is_empty() => f,
                    Ok(_) => return Err(format!("{} has no GitHub release in the list, and no package was found in {repo} \
                                                 (no .fwsc in it): {where_}", e.name)),
                    Err(err) => return Err(format!("{} has no GitHub release in the list, and {repo} could not be searched \
                                                    ({err}): {where_}", e.name)),
                }
            }
        };
        if files.is_empty() {
            return Err(format!("{repo} {} has no .fwsc package: {where_}", e.tag.as_deref().unwrap_or("")));
        }
        let pattern = e.asset_pkg.as_deref().and_then(|p| regex::Regex::new(&format!("^(?:{p})$")).ok());
        let hub_name = e.pkg.rsplit('/').next().unwrap_or("");
        let fm1 = |n: &str| { let n = n.to_lowercase(); n.contains("fm1") || n.contains("fm-1") };
        let mut order: Vec<String> = vec![];
        let ranks: [&dyn Fn(&str) -> bool; 4] = [
            &|n| n == hub_name,
            &|n| pattern.as_ref().is_some_and(|re| re.is_match(n)),
            &|n| fm1(n),
            &|_| true,
        ];
        for rank in ranks {
            for (n, u) in &files {
                if rank(n) && !order.contains(u) {
                    order.push(u.clone());
                }
            }
        }
        Ok(order)
    }

    /// Downloads the package and keeps it only if its SHA-256 matches the list
    /// and it is an FM-1 firmware package: the candidates in order, the first
    /// that matches (at most 6 tried).
    pub fn get(&self, e: &Entry, net: &dyn Fetch) -> Result<(PathBuf, String), String> {
        let urls = self.resolve(e, net)?;
        let mut tried = vec![];
        for url in urls.iter().take(6) {
            let data = net.get(url)?;
            let sha = sha256_hex(&data);
            if sha != e.sha256 || (e.size > 0 && data.len() as u64 != e.size) {
                tried.push((url.rsplit('/').next().unwrap_or(url).to_string(), sha));
                continue;
            }
            let pkg = Package::parse(&data).map_err(|err| format!("{} is not a firmware package: {}. Not kept.", e.name, err.0))?;
            if !matches!(pkg.model(), "FM-1" | "MiniFM") {
                return Err(format!("{} is a package for the {}, not the FM-1. Not kept.", e.name, pkg.model()));
            }
            let path = self.package_path(e);
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|err| format!("cannot create {}: {err}", self.dir.display()))?;
            write_whole(&path, &data)?;
            let mut index = self.index();
            index.insert(e.file_name(), (e.id.clone(), e.version.clone()));
            self.save_index(&index)?;
            return Ok((path, pkg.identity));
        }
        Err(match tried.as_slice() {
            [(name, sha)] => format!("{} ({name}) does not match the list: SHA-256 {sha}, the list says {}. Not kept.", e.name, e.sha256),
            _ => format!("{}: none of its {} packages ({}) matches the list's SHA-256 {}…: the author's files may be newer \
                          than the list (RELOAD tomorrow). Not kept.", e.name, tried.len(),
                         tried.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join(", "), &e.sha256[..e.sha256.len().min(8)]),
        })
    }

    fn stock_dir(&self) -> PathBuf {
        self.dir.join("stock")
    }

    /// The firmwares starred, by id.
    pub fn favorites(&self) -> std::collections::BTreeSet<String> {
        std::fs::read(self.dir.join("favorites.json")).ok()
            .and_then(|raw| serde_json::from_slice::<Vec<String>>(&raw).ok())
            .map(|v| v.into_iter().collect()).unwrap_or_default()
    }

    /// Stars or unstars a firmware.
    pub fn set_favorite(&self, id: &str, on: bool) -> Result<(), String> {
        let mut f = self.favorites();
        if on { f.insert(id.to_string()); } else { f.remove(id); }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let list: Vec<&String> = f.iter().collect();
        write_whole(&self.dir.join("favorites.json"), serde_json::to_string_pretty(&list).unwrap_or_default().as_bytes())
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("packages").join("index.json")
    }

    /// Which firmware and version each downloaded package is: file name ->
    /// (id, version). Packages saved before the index: none (they show as
    /// the list's current ones when their name is).
    fn index(&self) -> std::collections::BTreeMap<String, (String, String)> {
        let Ok(raw) = std::fs::read(self.index_path()) else { return Default::default() };
        let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        v.as_object().map(|o| o.iter().filter_map(|(file, e)| {
            Some((file.clone(), (s(e, "id")?, s(e, "version").unwrap_or_default())))
        }).collect()).unwrap_or_default()
    }

    fn save_index(&self, index: &std::collections::BTreeMap<String, (String, String)>) -> Result<(), String> {
        let v: serde_json::Map<String, Value> = index.iter()
            .map(|(f, (id, ver))| (f.clone(), serde_json::json!({"id": id, "version": ver}))).collect();
        write_whole(&self.index_path(), serde_json::to_string_pretty(&Value::Object(v)).unwrap_or_default().as_bytes())
    }

    /// Adds to the index the packages saved before it existed, named as the
    /// list's current versions (and checked against the list's SHA-256).
    pub fn index_saved(&self, cat: &Catalog) {
        let mut index = self.index();
        let before = index.len();
        for e in &cat.entries {
            if !index.contains_key(&e.file_name()) && self.downloaded(e) {
                index.insert(e.file_name(), (e.id.clone(), e.version.clone()));
            }
        }
        if index.len() != before {
            let _ = self.save_index(&index);
        }
    }

    /// The versions of a firmware on this computer other than the list's
    /// current one: an UPDATE is due when one is and the current one is not.
    pub fn older_versions(&self, e: &Entry) -> Vec<String> {
        let dir = self.dir.join("packages");
        self.index().into_iter()
            .filter(|(file, (id, ver))| id == &e.id && ver != &e.version && dir.join(file).exists())
            .map(|(_, (_, ver))| ver).collect()
    }

    /// Deletes every package of a firmware from this computer (each version,
    /// and the imported official package): returns the files deleted.
    pub fn delete(&self, id: &str) -> Result<Vec<PathBuf>, String> {
        let dir = self.dir.join("packages");
        let mut index = self.index();
        let mut files: Vec<String> = index.iter().filter(|(_, (i, _))| i == id).map(|(f, _)| f.clone()).collect();
        if let Ok(Some(cat)) = self.catalog() {
            if let Some(e) = cat.find(id) {
                if !files.contains(&e.file_name()) {
                    files.push(e.file_name());
                }
            }
        }
        let mut gone = vec![];
        for f in &files {
            let p = dir.join(f);
            match std::fs::remove_file(&p) {
                Ok(()) => gone.push(p),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("cannot delete {}: {err}", p.display())),
            }
            index.remove(f);
        }
        let stock = self.stock_dir().join(format!("{id}.fwsc"));
        if stock.exists() {
            std::fs::remove_file(&stock).map_err(|err| format!("cannot delete {}: {err}", stock.display()))?;
            gone.push(stock);
        }
        if dir.exists() {
            self.save_index(&index)?;
        }
        Ok(gone)
    }

    /// Copies M-VAVE's official packages from `from` (known by SHA-256);
    /// returns what it did with each .fwsc there.
    pub fn import_stock(&self, from: &Path) -> Result<Vec<String>, String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?
            .filter_map(Result::ok).map(|d| d.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("fwsc"))).collect();
        files.sort();
        if files.is_empty() {
            return Err(format!("no .fwsc files in {}", from.display()));
        }
        std::fs::create_dir_all(self.stock_dir()).map_err(|e| format!("cannot create {}: {e}", self.stock_dir().display()))?;
        let mut out = vec![];
        for f in files {
            let data = std::fs::read(&f).map_err(|e| format!("cannot read {}: {e}", f.display()))?;
            let sha = sha256_hex(&data);
            let shown = f.file_name().unwrap_or_default().to_string_lossy().to_string();
            match official(&sha) {
                Some((id, _, name)) => {
                    write_whole(&self.stock_dir().join(format!("{id}.fwsc")), &data)?;
                    out.push(format!("Imported {shown}: {name}, SHA-256 {}…", &sha[..8]));
                }
                None => out.push(format!("Not imported {shown}: not one of M-VAVE's official packages (SHA-256 {}…)", &sha[..8])),
            }
        }
        Ok(out)
    }

    /// The official packages imported: (id, version, name).
    pub fn imported(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        OFFICIAL.iter().filter(|o| {
            std::fs::read(self.stock_dir().join(format!("{}.fwsc", o.1))).is_ok_and(|d| sha256_hex(&d) == o.0)
        }).map(|o| (o.1, o.2, o.3)).collect()
    }

    /// The package to install for `id`: an imported official one, else the
    /// list's (downloaded first if it is not on disk yet).
    pub fn installable(&self, id: &str, net: &dyn Fetch) -> Result<Installable, String> {
        let p = self.stock_dir().join(format!("{id}.fwsc"));
        if let Ok(data) = std::fs::read(&p) {
            if let Some((oid, version, name)) = official(&sha256_hex(&data)) {
                return Ok(Installable { id: oid.into(), name: name.into(), version: version.into(), chip: "any".into(), data,
                                        official: true, credit: "M-VAVE".into(), path: p });
            }
        }
        let cat = self.catalog()?.ok_or("no firmware list yet: run `m-flasher library sync`")?;
        let e = cat.find(id).ok_or_else(|| format!("{id} is neither in the firmware list nor an imported official package"))?;
        if !self.downloaded(e) {
            println!("Downloading {} {} from {}", e.name, e.version, e.release.as_deref().or(e.source.as_deref()).unwrap_or("its author"));
            self.get(e, net)?;
        }
        let data = std::fs::read(self.package_path(e)).map_err(|err| format!("cannot read {}: {err}", self.package_path(e).display()))?;
        let sha = sha256_hex(&data);
        if sha != e.sha256 {
            return Err(format!("{} on disk no longer matches the list: download it again with `library get {}`", e.name, e.id));
        }
        Ok(Installable { id: e.id.clone(), name: e.name.clone(), version: e.version.clone(), chip: e.chip.clone(), data,
                         official: official(&sha).is_some(),
                         credit: format!("{} by {}, {}", e.name, if e.author.is_empty() { "its author" } else { &e.author }, e.license),
                         path: self.package_path(e) })
    }
}

/// `m-flasher library ...`
pub fn cli(args: &[String]) -> Result<(), String> {
    let lib = Library::new(default_dir());
    let (mut chip, mut kind, mut beta, mut pos) = (None, None, false, vec![]);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--chip" => chip = Some(it.next().ok_or(format!("--chip needs old or new\n{USAGE}"))?.to_lowercase()),
            "--kind" => kind = Some(it.next().ok_or(format!("--kind needs a value\n{USAGE}"))?.to_lowercase()),
            "--beta" => beta = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            s => pos.push(s.to_string()),
        }
    }
    if chip.as_deref().is_some_and(|c| c != "old" && c != "new") {
        return Err(format!("--chip is old (0x856014) or new (0xB36014)\n{USAGE}"));
    }
    let need_catalog = || lib.catalog()?.ok_or_else(|| "no firmware list yet: run `m-flasher library sync`".to_string());
    match pos.first().map(String::as_str) {
        Some("sync") => {
            match lib.sync(&Http::new(), chrono::Utc::now())? {
                Synced::Fetched => println!("Fetched the firmware list."),
                Synced::Recent(at) => println!("The firmware list from {at} is less than a day old: not fetched again (the hub asks for one read a day)."),
            }
            let cat = need_catalog()?;
            println!("{} firmwares (list updated {}). Saved in {}", cat.entries.len(), cat.updated, lib.dir.display());
            for p in lib.orphans(&cat) {
                println!("No longer in the list: {} (delete it if you no longer want it)", p.display());
            }
            println!("{ATTRIBUTION}");
            Ok(())
        }
        Some("list") => {
            let cat = need_catalog()?;
            let imported = lib.imported();
            if !imported.is_empty() {
                println!("Official M-VAVE, imported:");
                for (id, version, name) in &imported {
                    println!("{:<10} {:<22} {:<16} {:<14} {:<5} {:<12} {}", "saved", id, version, "stock", "any", "M-VAVE", name);
                }
                println!();
            }
            println!("{ATTRIBUTION}\nList updated {}{}\n", cat.updated, lib.fetched_at().map(|t| format!(", fetched {}", t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))).unwrap_or_default());
            let mut shown = 0;
            for e in &cat.entries {
                if e.beta && !beta || kind.as_deref().is_some_and(|k| !e.kinds.iter().any(|x| x.eq_ignore_ascii_case(k))) {
                    continue;
                }
                let state = if lib.downloaded(e) { "saved" } else if e.elsewhere { "elsewhere" } else if !e.fits_chip(chip.as_deref()) { "old chip" } else { "" };
                println!("{:<10} {:<22} {:<16} {:<14} {:<5} {:<12} {}{}", state, e.id, e.version, e.kinds.join(","), e.chip, e.license, e.name,
                         if e.beta { " (beta)" } else { "" });
                shown += 1;
            }
            println!("\n{shown} shown{}", if beta { "" } else { "; --beta adds the betas" });
            if chip.as_deref() == Some("new") {
                println!("\"old chip\": the list marks it for the FM-1's older flash chip (0x856014) only: not for this unit.");
            }
            Ok(())
        }
        Some("get") => {
            let id = pos.get(1).ok_or(format!("get needs a firmware's id (see `library list`)\n{USAGE}"))?;
            let cat = need_catalog()?;
            let e = cat.find(id).ok_or(format!("{id} is not in the firmware list"))?;
            if lib.downloaded(e) {
                println!("{} {} is already saved and matches the list: {}", e.name, e.version, lib.package_path(e).display());
                return Ok(());
            }
            println!("Downloading {} {} from {}", e.name, e.version, e.release.as_deref().or(e.source.as_deref()).unwrap_or("its author"));
            let (path, identity) = lib.get(e, &Http::new())?;
            println!("Saved {path}: {identity}, SHA-256 matches the list ({}).", e.sha256, path = path.display());
            println!("{} by {}, {}. {ATTRIBUTION}", e.name, if e.author.is_empty() { "its author" } else { &e.author }, e.license);
            Ok(())
        }
        Some("import-stock") => {
            let from = pos.get(1).ok_or(format!("import-stock needs the folder with the .fwsc files\n{USAGE}"))?;
            for line in lib.import_stock(Path::new(from))? {
                println!("{line}");
            }
            Ok(())
        }
        Some("install") => Err("install talks to the unit: run `m-flasher library install ID`".into()),
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct Table {
        urls: HashMap<String, Vec<u8>>,
        asked: RefCell<Vec<String>>,
    }

    impl Fetch for Table {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.asked.borrow_mut().push(url.into());
            self.urls.get(url).cloned().ok_or_else(|| format!("{url}: 404"))
        }
    }

    fn table(pairs: &[(&str, Vec<u8>)]) -> Table {
        Table { urls: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), asked: RefCell::new(vec![]) }
    }

    const STOCK_SHA: &str = "db1642b2b6fa5c2cccb11ffd13878068bb28601678d3644049f99dc40e7edb8a";

    fn catalog_json(fake_sha: &str) -> Vec<u8> {
        serde_json::json!({
            "license": {"license": "CC-BY-4.0"},
            "updated": "2026-11-15 21:02 UTC",
            "stock": {"pkg": "fw/mvave/FM-1_015.fwsc", "sha256": STOCK_SHA, "size": 699956, "product": "FM-1_015",
                      "source": "https://github.com/m-vave/fm1_open/blob/b178ac4/sdk/firmware/FM-1_015.fwsc"},
            "firmwares": [
                {"id": "felucca", "name": "Felucca", "version": "1.8.2", "author": "hugelton", "repo": "hugelton/Felucca",
                 "tag": "v1.8.2", "release": "https://github.com/hugelton/Felucca/releases/tag/v1.8.2",
                 "assetPkg": "felucca-[\\d.]+\\.fwsc", "pkg": "fw/felucca-1.8.2.fwsc", "sha256": fake_sha,
                 "chip": "new", "lic": "GPL-3.0", "kind": ["groove", "dev"], "beta": false},
                {"id": "zp12", "name": "zp12", "version": "2.5", "repo": "zvenson/zp12", "pkg": "fw/zp12-2.5.fwsc",
                 "sha256": "00", "chip": "old", "lic": "GPL-3.0", "kind": "drums"},
                {"id": "baudgirl", "name": "FM-1+VA", "version": "1", "pkg": "fw/x.fwsc", "sha256": "00", "linkOnly": true,
                 "release": "https://example.org/baud"},
                {"name": "no id", "pkg": "fw/y.fwsc", "sha256": "00"}
            ]
        }).to_string().into_bytes()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mflr-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-11-16T01:30:00Z").unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn the_search_matches_every_word_in_any_text() {
        let sloop = ["SLOOP", "sloop", "? · GPL-3.0", "groove", "2.8.5"];
        let felucca = ["Felucca", "felucca", "Leo Kuroshita (Hügelton Instruments) · GPL-3.0", "groove dev", "1.8.2"];
        assert!(matches_search("", &sloop), "an empty search shows everything");
        assert!(matches_search("sloop", &sloop) && matches_search("SLO", &sloop));
        assert!(matches_search("groove 2.8", &sloop));
        assert!(!matches_search("sloop", &felucca));
        assert!(matches_search("hügelton", &felucca) && matches_search("kuroshita dev", &felucca));
        assert!(!matches_search("hsloop", &sloop) && !matches_search("sloop drums", &sloop));
    }

    #[test]
    fn parses_the_list_with_the_stock_firmware_first() {
        let c = Catalog::parse(&catalog_json("ab")).unwrap();
        let ids: Vec<&str> = c.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["mvave-015", "felucca", "zp12", "baudgirl"]);
        let st = &c.entries[0];
        assert!(st.stock && st.version == "015" && st.sha256 == STOCK_SHA);
        let f = c.find("Felucca").unwrap();
        assert_eq!(f.kinds, ["groove", "dev"]);
        assert_eq!(c.find("zp12").unwrap().kinds, ["drums"]);
        assert!(c.find("baudgirl").unwrap().elsewhere);
        assert_eq!(f.file_name(), "felucca-1.8.2.fwsc");
    }

    #[test]
    fn the_older_chips_firmware_is_not_for_a_new_chip() {
        let c = Catalog::parse(&catalog_json("ab")).unwrap();
        let zp = c.find("zp12").unwrap();
        assert!(zp.fits_chip(Some("old")) && zp.fits_chip(None) && !zp.fits_chip(Some("new")));
        assert!(c.find("felucca").unwrap().fits_chip(Some("new")));
    }

    #[test]
    fn the_list_is_fetched_at_most_once_a_day() {
        let lib = Library::new(tmp("sync"));
        let net = table(&[(CATALOG_URL, catalog_json("ab"))]);
        assert_eq!(lib.sync(&net, now()).unwrap(), Synced::Fetched);
        assert!(matches!(lib.sync(&net, now() + chrono::TimeDelta::hours(23)).unwrap(), Synced::Recent(_)));
        assert_eq!(net.asked.borrow().len(), 1);
        assert_eq!(lib.sync(&net, now() + chrono::TimeDelta::hours(25)).unwrap(), Synced::Fetched);
        assert_eq!(net.asked.borrow().len(), 2);
        assert_eq!(lib.catalog().unwrap().unwrap().entries.len(), 4);
    }

    #[test]
    fn a_bad_list_does_not_replace_the_one_on_disk() {
        let lib = Library::new(tmp("bad"));
        lib.sync(&table(&[(CATALOG_URL, catalog_json("ab"))]), now()).unwrap();
        let later = now() + chrono::TimeDelta::hours(30);
        assert!(lib.sync(&table(&[(CATALOG_URL, b"<html>".to_vec())]), later).is_err());
        assert_eq!(lib.catalog().unwrap().unwrap().entries.len(), 4);
    }

    #[test]
    fn downloads_come_from_the_authors_release_never_the_hub() {
        let lib = Library::new(tmp("resolve"));
        let c = Catalog::parse(&catalog_json("ab")).unwrap();
        let rel = serde_json::json!({"assets": [
            {"name": "felucca-1.8.2.zip", "browser_download_url": "https://github.com/dl/zip"},
            {"name": "felucca-1.8.2.fwsc", "browser_download_url": "https://github.com/dl/fwsc"}]}).to_string().into_bytes();
        let net = table(&[("https://api.github.com/repos/hugelton/Felucca/releases/tags/v1.8.2", rel)]);
        assert_eq!(lib.resolve(c.find("felucca").unwrap(), &net).unwrap(), ["https://github.com/dl/fwsc"], "only the .fwsc");
        assert_eq!(lib.resolve(&c.entries[0], &net).unwrap(),
                   ["https://raw.githubusercontent.com/m-vave/fm1_open/b178ac4/sdk/firmware/FM-1_015.fwsc"]);
        assert!(lib.resolve(c.find("baudgirl").unwrap(), &net).unwrap_err().contains("own installer"));
        assert!(lib.resolve(c.find("zp12").unwrap(), &net).unwrap_err().contains("no GitHub release"));
        assert!(net.asked.borrow().iter().all(|u| !u.contains("thefirmwarehub.com")));
    }

    #[test]
    fn without_a_release_the_package_comes_from_the_authors_repository() {
        let lib = Library::new(tmp("repofile"));
        let c = Catalog::parse(&catalog_json("ab")).unwrap();
        let zp = c.find("zp12").unwrap();
        let tree = serde_json::json!({"tree": [
            {"path": "README.md", "type": "blob"},
            {"path": "docs/install/firmware/zp12-2.5.fwsc", "type": "blob"},
            {"path": "old/zp12-2.4.fwsc", "type": "blob"}]}).to_string().into_bytes();
        let net = table(&[("https://api.github.com/repos/zvenson/zp12", br#"{"default_branch": "main"}"#.to_vec()),
                          ("https://api.github.com/repos/zvenson/zp12/git/trees/main?recursive=1", tree)]);
        assert_eq!(lib.resolve(zp, &net).unwrap()[0],
                   "https://raw.githubusercontent.com/zvenson/zp12/main/docs/install/firmware/zp12-2.5.fwsc",
                   "the file named as on the hub first, before another version");
        assert!(net.asked.borrow().iter().all(|u| !u.contains("thefirmwarehub.com")));
        let empty = table(&[("https://api.github.com/repos/zvenson/zp12", br#"{"default_branch": "main"}"#.to_vec()),
                            ("https://api.github.com/repos/zvenson/zp12/git/trees/main?recursive=1", br#"{"tree": []}"#.to_vec())]);
        assert!(lib.resolve(zp, &empty).unwrap_err().contains("no .fwsc in it"));
    }

    #[test]
    fn of_several_packages_the_one_matching_the_sha256_is_kept() {
        let Some(dir) = std::env::var_os("FLASHER_TEST_DATA") else { return };
        let Ok(real) = std::fs::read(PathBuf::from(dir).join("fm1-015.fwsc")) else { return };
        let lib = Library::new(tmp("several"));
        let mut c = Catalog::parse(&catalog_json(&sha256_hex(&real))).unwrap();
        let e = c.entries.iter_mut().find(|e| e.id == "felucca").unwrap();
        e.asset_pkg = None;
        e.pkg = "fw/fmini-1-2.6.0.fwsc".into();
        e.size = 0;
        let rel = serde_json::json!({"assets": [
            {"name": "fmini-1-smk37-2.6.0.fwsc", "browser_download_url": "https://github.com/dl/smk37"},
            {"name": "fmini-1-fm1-2.6.0.fwsc", "browser_download_url": "https://github.com/dl/fm1"},
            {"name": "fmini-1-smk37-elite-2.6.0.fwsc", "browser_download_url": "https://github.com/dl/elite"}]}).to_string().into_bytes();
        let net = table(&[("https://api.github.com/repos/hugelton/Felucca/releases/tags/v1.8.2", rel),
                          ("https://github.com/dl/smk37", b"smk37 build".to_vec()),
                          ("https://github.com/dl/fm1", real.clone()),
                          ("https://github.com/dl/elite", b"elite build".to_vec())]);
        let e = c.find("felucca").unwrap().clone();
        assert_eq!(lib.resolve(&e, &net).unwrap()[0], "https://github.com/dl/fm1", "the FM-1 build is tried first");
        let (path, _) = lib.get(&e, &net).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), real);
        assert!(!net.asked.borrow().iter().any(|u| u.ends_with("/elite")), "stops at the one that matches");
    }

    #[test]
    fn an_older_version_on_disk_means_an_update_and_delete_removes_every_version() {
        let Some(dir) = std::env::var_os("FLASHER_TEST_DATA") else { return };
        let Ok(real) = std::fs::read(PathBuf::from(dir).join("fm1-015.fwsc")) else { return };
        let lib = Library::new(tmp("update"));
        let rel = |url: &str| serde_json::json!({"assets": [{"name": "felucca-1.8.2.fwsc", "browser_download_url": url}]}).to_string().into_bytes();
        // version 1.8.2 downloaded
        let c = Catalog::parse(&catalog_json(&sha256_hex(&real))).unwrap();
        let mut e = c.find("felucca").unwrap().clone();
        e.size = 0;
        let net = table(&[("https://api.github.com/repos/hugelton/Felucca/releases/tags/v1.8.2", rel("https://github.com/dl/a")),
                          ("https://github.com/dl/a", real.clone())]);
        lib.get(&e, &net).unwrap();
        assert!(lib.downloaded(&e) && lib.older_versions(&e).is_empty());
        // the list moves to 1.9: 1.8.2 is older, an update is due
        let mut newer = e.clone();
        newer.version = "1.9".into();
        assert!(!lib.downloaded(&newer));
        assert_eq!(lib.older_versions(&newer), ["1.8.2"]);
        // a firmware whose id starts the same is not mistaken for it
        let mut other = e.clone();
        other.id = "felucca8".into();
        assert!(lib.older_versions(&other).is_empty());
        // delete: every version of it goes, and nothing else
        std::fs::create_dir_all(lib.dir.join("stock")).unwrap();
        std::fs::write(lib.dir.join("stock").join("mvave-016.fwsc"), b"x").unwrap();
        let gone = lib.delete("felucca").unwrap();
        assert_eq!(gone.len(), 1);
        assert!(!lib.downloaded(&e) && lib.older_versions(&newer).is_empty());
        assert!(lib.dir.join("stock").join("mvave-016.fwsc").exists());
        assert_eq!(lib.delete("mvave-016").unwrap().len(), 1, "an imported official package too");
    }

    #[test]
    fn favorites_are_kept_on_disk() {
        let lib = Library::new(tmp("favorites"));
        assert!(lib.favorites().is_empty());
        lib.set_favorite("choralroot", true).unwrap();
        lib.set_favorite("felucca", true).unwrap();
        lib.set_favorite("felucca", false).unwrap();
        let again = Library::new(lib.dir.clone());
        assert_eq!(again.favorites().into_iter().collect::<Vec<_>>(), ["choralroot"]);
    }

    #[test]
    fn a_package_that_does_not_match_the_list_is_not_kept() {
        let lib = Library::new(tmp("mismatch"));
        let body = b"not the package".to_vec();
        let c = Catalog::parse(&catalog_json(&sha256_hex(b"something else"))).unwrap();
        let rel = serde_json::json!({"assets": [{"name": "felucca-1.8.2.fwsc", "browser_download_url": "https://github.com/dl/f"}]}).to_string().into_bytes();
        let net = table(&[("https://api.github.com/repos/hugelton/Felucca/releases/tags/v1.8.2", rel.clone()), ("https://github.com/dl/f", body.clone())]);
        let e = c.find("felucca").unwrap();
        assert!(lib.get(e, &net).unwrap_err().contains("does not match the list"));
        assert!(!lib.package_path(e).exists());
        // the right hash, but not a firmware package: refused all the same
        let c = Catalog::parse(&catalog_json(&sha256_hex(&body))).unwrap();
        assert!(lib.get(c.find("felucca").unwrap(), &net).unwrap_err().contains("not a firmware package"));
        assert!(!lib.package_path(e).exists());
    }

    #[test]
    fn the_stock_package_is_downloaded_checked_and_kept() {
        let Some(dir) = std::env::var_os("FLASHER_TEST_DATA") else { return };
        let Ok(data) = std::fs::read(PathBuf::from(dir).join("fm1-015.fwsc")) else {
            eprintln!("skipped: fm1-015.fwsc not in FLASHER_TEST_DATA");
            return;
        };
        let lib = Library::new(tmp("stock"));
        let c = Catalog::parse(&catalog_json("ab")).unwrap();
        let net = table(&[("https://raw.githubusercontent.com/m-vave/fm1_open/b178ac4/sdk/firmware/FM-1_015.fwsc", data)]);
        let (path, identity) = lib.get(&c.entries[0], &net).unwrap();
        assert_eq!(identity, "FM-1_015");
        assert!(lib.downloaded(&c.entries[0]) && path.ends_with("mvave-015-015.fwsc"));
        assert!(lib.orphans(&c).is_empty());
    }

    #[test]
    fn official_packages_are_imported_by_their_hash_only() {
        let Some(data) = std::env::var_os("FLASHER_TEST_DATA") else { return };
        let data = PathBuf::from(data);
        let (Ok(a), Ok(b)) = (std::fs::read(data.join("fm1-015.fwsc")), std::fs::read(data.join("fm1-016.fwsc"))) else {
            eprintln!("skipped: fm1-015.fwsc or fm1-016.fwsc not in FLASHER_TEST_DATA");
            return;
        };
        let from = tmp("import-from");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("FM-1_015.fwsc"), &a).unwrap();
        std::fs::write(from.join("FM-1_016.fwsc"), &b).unwrap();
        let mut changed = b.clone();
        changed[1000] ^= 1;
        std::fs::write(from.join("FM-1_016 (copy).fwsc"), &changed).unwrap();
        let lib = Library::new(tmp("import"));
        let lines = lib.import_stock(&from).unwrap();
        assert_eq!(lines.iter().filter(|l| l.starts_with("Imported")).count(), 2, "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Not imported FM-1_016 (copy).fwsc")), "{lines:?}");
        let ids: Vec<&str> = lib.imported().iter().map(|i| i.0).collect();
        assert_eq!(ids, ["mvave-015", "mvave-016"]);
        let i = lib.installable("mvave-016", &table(&[])).unwrap();
        assert!(i.official && i.version == "016" && i.data == b);
    }
}
