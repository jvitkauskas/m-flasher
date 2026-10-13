//! The window's fonts, and the system's fonts for letters they lack: file and
//! folder names can be in any script (a Windows user folder in Cyrillic or
//! Chinese, say). A text with such letters makes the app look for a system
//! font that has them, once; the window draws them from the next frame on.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use skrifa::{FontRef, MetadataProvider};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Our fonts, by family name; IBM Plex Mono (Latin, Cyrillic) follows each.
const OURS: [(&str, &[u8]); 4] = [
    ("cond", include_bytes!("../assets/BarlowCondensed-SemiBold.ttf")),
    ("tech", include_bytes!("../assets/ShareTechMono-Regular.ttf")),
    ("plex", include_bytes!("../assets/IBMPlexMono-Regular.ttf")),
    ("lcd", include_bytes!("../assets/VT323-Regular.ttf")),
];
const PLEX: &[u8] = OURS[2].1;

fn has(data: &[u8], index: u32, c: char) -> bool {
    FontRef::from_index(data, index).is_ok_and(|f| f.charmap().map(c).is_some())
}

/// Where the baseline of `fallback` must move (in font sizes, down) to sit on
/// that of `primary`: egui centres a fallback font's line box on the primary
/// font's, which leaves fonts with other proportions (a CJK font's large line
/// gap) too high or too low.
fn baseline_offset(primary: &FontRef, fallback: &FontRef) -> f32 {
    use skrifa::instance::{LocationRef, Size};
    let mid = |f: &FontRef| {
        let m = f.metrics(Size::unscaled(), LocationRef::default());
        (m.ascent + m.descent - m.leading) / m.units_per_em.max(1) as f32
    };
    0.5 * (mid(primary) - mid(fallback))
}

/// A system font in use: its file's contents (kept for the app's lifetime)
/// and the font's index in it (a .ttc holds several).
struct Loaded {
    path: PathBuf,
    index: u32,
    data: &'static [u8],
    /// It has Chinese characters: such fonts come last, as their Greek or
    /// Cyrillic letters are often full width.
    han: bool,
    /// Its place in the order of preference.
    rank: usize,
}

#[derive(Default)]
pub struct Fonts {
    loaded: Vec<Loaded>,
    /// Letters already looked for, found or not.
    searched: HashSet<char>,
}

impl Fonts {
    pub fn install(&self, ctx: &egui::Context) {
        let mut fonts = FontDefinitions::default();
        // egui's own fonts last, for symbols (the check mark, the arrows)
        let egui_fonts = fonts.families[&FontFamily::Proportional].clone();
        let plex = FontRef::new(PLEX).ok();
        for (name, bytes) in OURS {
            fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
            let Ok(primary) = FontRef::new(bytes) else { continue };
            // the fallbacks, each moved onto this font's baseline
            let mut fallbacks = vec![];
            if name != "plex" {
                fallbacks.push(("plex".to_owned(), PLEX, 0, plex.as_ref().map_or(0.0, |f| baseline_offset(&primary, f))));
            }
            for (k, l) in self.loaded.iter().enumerate() {
                let off = FontRef::from_index(l.data, l.index).map_or(0.0, |f| baseline_offset(&primary, &f));
                fallbacks.push((format!("system{k}"), l.data, l.index, off));
            }
            let mut list = vec![name.to_owned()];
            for (fallback, data, index, off) in fallbacks {
                let id = format!("{fallback}@{name}");
                let tweak = egui::FontTweak { y_offset_factor: off, ..Default::default() };
                fonts.font_data.insert(id.clone(), Arc::new(FontData { index, tweak, ..FontData::from_static(data) }));
                list.push(id);
            }
            list.extend(egui_fonts.iter().cloned());
            fonts.families.insert(FontFamily::Name(name.into()), list);
        }
        ctx.set_fonts(fonts);
    }

    /// The letters of `text` that the family's fonts lack and that have not
    /// been looked for yet.
    pub fn missing(&self, family: &str, text: &str, out: &mut Vec<char>) {
        if text.is_ascii() {
            return;
        }
        let own = OURS.iter().find(|(n, _)| *n == family).map_or(PLEX, |(_, b)| *b);
        for c in text.chars() {
            // letters and digits only: egui's fonts draw the symbols
            if c.is_alphanumeric() && !self.searched.contains(&c) && !has(own, 0, c) && !has(PLEX, 0, c) && !out.contains(&c) {
                out.push(c);
            }
        }
    }

    /// Finds each letter's system font: the first, in the order of preference,
    /// that has it. True if a font was added (the fonts must then be installed
    /// again).
    pub fn search(&mut self, letters: &[char]) -> bool {
        let need: Vec<char> = letters.iter().copied().filter(|c| self.searched.insert(*c)).collect();
        let before = self.loaded.len();
        // the files read in this search: each is read once
        let mut files: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
        for c in need {
            for (rank, (path, index)) in candidates(c).into_iter().enumerate() {
                if let Some(l) = self.loaded.iter().find(|l| l.path == path && l.index == index) {
                    if has(l.data, index, c) {
                        break;
                    }
                    continue;
                }
                let bytes = files.entry(path.clone()).or_insert_with(|| std::fs::read(&path).ok());
                if !bytes.as_deref().is_some_and(|b| has(b, index, c)) {
                    continue;
                }
                let data: &'static [u8] = Box::leak(bytes.take().unwrap_or_default().into_boxed_slice());
                files.remove(&path); // taken: read again if another font in it is needed
                let han = has(data, index, '\u{4E00}');
                self.loaded.push(Loaded { path, index, data, han, rank });
                break;
            }
        }
        // a letter is drawn by the first font that has it
        self.loaded.sort_by_key(|l| (l.han, l.rank));
        self.loaded.len() > before
    }
}

/// System fonts that may have the letter, best first.
#[cfg(windows)]
fn candidates(_: char) -> Vec<(PathBuf, u32)> {
    let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    [
        "segoeui.ttf",  // Latin, Greek, Cyrillic, Hebrew, Arabic, Georgian, Armenian
        "msyh.ttc",     // Microsoft YaHei: Chinese, Japanese kana
        "YuGothR.ttc",  // Yu Gothic: Japanese
        "malgun.ttf",   // Malgun Gothic: Korean
        "Nirmala.ttc",  // Nirmala UI: Indian scripts (Windows 11)
        "Nirmala.ttf",  // (Windows 10)
        "LeelawUI.ttf", // Leelawadee UI: Thai, Lao, Khmer
        "ebrima.ttf",   // Ebrima: African scripts
        "seguisym.ttf", // Segoe UI Symbol
        "simsun.ttc",   // SimSun: rarer Chinese characters
    ]
    .iter()
    .map(|f| (dir.join(f), 0))
    .collect()
}

#[cfg(target_os = "macos")]
fn candidates(_: char) -> Vec<(PathBuf, u32)> {
    [
        "Helvetica.ttc",               // Latin, Greek, Cyrillic
        "Hiragino Sans GB.ttc",        // Chinese, Japanese kana
        "ヒラギノ角ゴシック W3.ttc",  // Japanese
        "AppleSDGothicNeo.ttc",        // Korean
        "ArialHB.ttc",                 // Hebrew
        "GeezaPro.ttc",                // Arabic
        "Kohinoor.ttc",                // Devanagari
        "Thonburi.ttc",                // Thai
        "Arial Unicode.ttf",           // most of the rest
    ]
    .iter()
    .flat_map(|f| ["/System/Library/Fonts", "/System/Library/Fonts/Supplemental"].map(|d| (PathBuf::from(d).join(f), 0)))
    .collect()
}

/// Linux: fontconfig's choice for the letter.
#[cfg(not(any(windows, target_os = "macos")))]
fn candidates(c: char) -> Vec<(PathBuf, u32)> {
    let Ok(out) = std::process::Command::new("fc-match")
        .args(["-s", "--format=%{file}\\t%{index}\\n", &format!(":charset={:x}", c as u32)])
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .take(8)
        .filter_map(|l| l.split_once('\t'))
        .map(|(f, i)| (PathBuf::from(f), i.parse().unwrap_or(0)))
        .collect()
}
