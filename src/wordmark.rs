//! The "M-FLASHER" wordmark, after 1980s synthesizer lettering: the M cut
//! into horizontal stripes, "-FLASHER" in outline. Drawn from the letter
//! outlines of Saira Expanded Black Italic (OFL; a static instance with the
//! overlapping contours merged, cut down to these letters: assets/README.md),
//! so it stays sharp at any window size.

use eframe::egui::{Color32, Mesh, Pos2, Rect, Shape, Stroke, pos2};
use lyon_tessellation::path::Path;
use lyon_tessellation::{BuffersBuilder, FillOptions, FillTessellator, FillVertex, VertexBuffers};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider};
use std::sync::OnceLock;

const FONT: &[u8] = include_bytes!("../assets/SairaExpanded-BlackItalic-wordmark.ttf");
const TEXT: &str = "M-FLASHER";
/// Stripes in the M, and how much of each period is stripe.
const STRIPES: usize = 6;
const FILL: f32 = 0.68;
/// The outline's width, in cap heights.
const OUTLINE: f32 = 0.054;

/// The word in font units (y up, the baseline at 0, the M at x 0).
struct Word {
    cap: f32,
    width: f32,
    /// The M's outline, and its fill as triangles.
    m: Vec<Vec<[f32; 2]>>,
    m_fill: Vec<[f32; 2]>,
    /// "-FLASHER": its outlines.
    rest: Vec<Vec<[f32; 2]>>,
}

/// Collects a glyph's contours, curves flattened to short lines.
struct Contours {
    dx: f32,
    all: Vec<Vec<[f32; 2]>>,
}

impl Contours {
    fn last(&self) -> [f32; 2] {
        *self.all.last().and_then(|c| c.last()).unwrap_or(&[0.0, 0.0])
    }
    fn push(&mut self, x: f32, y: f32) {
        let p = [x + self.dx, y];
        if let Some(c) = self.all.last_mut() {
            c.push(p);
        }
    }
}

const STEPS: usize = 10;

impl OutlinePen for Contours {
    fn move_to(&mut self, x: f32, y: f32) {
        self.all.push(vec![[x + self.dx, y]]);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.push(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let [x0, y0] = self.last();
        let (cx, cy) = (cx + self.dx, cy);
        for i in 1..=STEPS {
            let t = i as f32 / STEPS as f32;
            let u = 1.0 - t;
            let px = u * u * x0 + 2.0 * u * t * cx + t * t * (x + self.dx);
            let py = u * u * y0 + 2.0 * u * t * cy + t * t * y;
            self.push(px - self.dx, py);
        }
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let [x0, y0] = self.last();
        let (ax, bx, ex) = (cx0 + self.dx, cx1 + self.dx, x + self.dx);
        for i in 1..=STEPS {
            let t = i as f32 / STEPS as f32;
            let u = 1.0 - t;
            let px = u * u * u * x0 + 3.0 * u * u * t * ax + 3.0 * u * t * t * bx + t * t * t * ex;
            let py = u * u * u * y0 + 3.0 * u * u * t * cy0 + 3.0 * u * t * t * cy1 + t * t * t * y;
            self.push(px - self.dx, py);
        }
    }
    fn close(&mut self) {
        if let Some(c) = self.all.last_mut() {
            if c.len() > 1 && c.first() == c.last() {
                c.pop();
            }
        }
    }
}

fn word() -> &'static Word {
    static WORD: OnceLock<Word> = OnceLock::new();
    WORD.get_or_init(|| {
        let font = FontRef::new(FONT).expect("the wordmark font");
        let (size, location) = (Size::unscaled(), LocationRef::default());
        let charmap = font.charmap();
        let outlines = font.outline_glyphs();
        let advances = font.glyph_metrics(size, location);
        let mut x = 0.0;
        let (mut m, mut rest) = (vec![], vec![]);
        for (i, c) in TEXT.chars().enumerate() {
            let Some(gid) = charmap.map(c) else { continue };
            let mut pen = Contours { dx: x, all: vec![] };
            if let Some(glyph) = outlines.get(gid) {
                let _ = glyph.draw(DrawSettings::unhinted(size, location), &mut pen);
            }
            if i == 0 { m.extend(pen.all) } else { rest.extend(pen.all) }
            x += advances.advance_width(gid).unwrap_or(0.0);
        }
        // the M's fill
        let mut path = Path::builder();
        for c in &m {
            path.begin(c[0].into());
            for p in &c[1..] {
                path.line_to((*p).into());
            }
            path.end(true);
        }
        let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
        let _ = FillTessellator::new().tessellate_path(
            &path.build(),
            &FillOptions::non_zero(),
            &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| v.position().to_array()),
        );
        let m_fill = buffers.indices.iter().map(|&i| buffers.vertices[i as usize]).collect();
        let cap = font.metrics(size, location).cap_height.unwrap_or(700.0);
        // the width to the right edge of the last letter's ink
        let width = rest.iter().flatten().chain(m.iter().flatten()).map(|p| p[0]).fold(0.0, f32::max);
        Word { cap, width, m, m_fill, rest }
    })
}

/// Draws the wordmark with its left end at `x` and its baseline at `baseline`,
/// letters `cap` tall (screen points), in `color`; `clip` is the painter's
/// clip. Returns its width.
pub fn draw(painter: &eframe::egui::Painter, x: f32, baseline: f32, cap: f32, color: Color32) -> f32 {
    let w = word();
    let k = cap / w.cap;
    let at = |p: &[f32; 2]| pos2(x + p[0] * k, baseline - p[1] * k);
    // the M: its fill, and a thin outline to smooth the fill's edges, in each stripe
    let mut mesh = Mesh::default();
    for t in w.m_fill.chunks(3) {
        let i = mesh.vertices.len() as u32;
        for p in t {
            mesh.colored_vertex(at(p), color);
        }
        mesh.add_triangle(i, i + 1, i + 2);
    }
    let period = cap / (FILL + STRIPES as f32 - 1.0);
    let m_right = w.m.iter().flatten().map(|p| at(p).x).fold(x, f32::max);
    for i in 0..STRIPES {
        let top = baseline - cap + i as f32 * period;
        let band = Rect::from_min_max(pos2(x - cap, top), pos2(m_right + 1.0, top + FILL * period));
        let p = painter.with_clip_rect(band.intersect(painter.clip_rect()));
        p.add(Shape::mesh(mesh.clone()));
        for c in &w.m {
            p.add(Shape::closed_line(c.iter().map(at).collect::<Vec<Pos2>>(), Stroke::new(0.8, color)));
        }
    }
    // "-FLASHER"
    let stroke = Stroke::new(OUTLINE * cap, color);
    for c in &w.rest {
        painter.add(Shape::closed_line(c.iter().map(at).collect(), stroke));
    }
    w.width * k
}
