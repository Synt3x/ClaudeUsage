//! Drawing: shapes with tiny-skia, text with the Windows fonts (Segoe UI, Segoe Fluent Icons).

use crate::logo::{fill_path, rounded_rect};
use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};
use tiny_skia::{Color, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

pub type Rgb = (u8, u8, u8);

// Colors (dark mode), same as the Python version
pub const CLAY: Rgb = (0xD9, 0x77, 0x57);
pub const AMBER: Rgb = (0xE8, 0xA3, 0x3D);
pub const RED: Rgb = (0xE5, 0x53, 0x4B);
pub const BG: Rgb = (0x26, 0x26, 0x24);
pub const BG2: Rgb = (0x30, 0x30, 0x2E);
pub const TRACK: Rgb = (0x3A, 0x3A, 0x37);
pub const FG: Rgb = (0xEE, 0xEE, 0xEA);
pub const FG2: Rgb = (0xC9, 0xC9, 0xC4);
pub const MUTED: Rgb = (0x9A, 0x9A, 0x95);
pub const DARK: Rgb = (44, 18, 8);
pub const INK: Rgb = (0x1b, 0x1b, 0x1b);
pub const WHITE: Rgb = (255, 255, 255);

fn arr(c: Rgb) -> [u8; 3] {
    [c.0, c.1, c.2]
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Face {
    Reg,
    Bold,
    Icon,
}

/// A font face at an em size in pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Spec {
    pub face: Face,
    pub em: f32,
}

pub struct Fonts {
    reg: Option<FontVec>,
    bold: Option<FontVec>,
    icon: Option<FontVec>,
    fallback: Vec<FontVec>,
}

fn load_font(names: &[&str]) -> Option<FontVec> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    names.iter().find_map(|n| {
        let data = std::fs::read(dir.join("Fonts").join(n)).ok()?;
        FontVec::try_from_vec(data).ok()
    })
}

impl Fonts {
    pub fn load() -> Fonts {
        Fonts {
            reg: load_font(&["segoeui.ttf", "arial.ttf"]),
            bold: load_font(&["segoeuib.ttf", "arialbd.ttf"]),
            icon: load_font(&["SegoeIcons.ttf", "segmdl2.ttf"]),
            fallback: ["seguisym.ttf", "segoeui.ttf"].iter().filter_map(|n| load_font(&[n])).collect(),
        }
    }

    fn face(&self, f: Face) -> Option<&FontVec> {
        match f {
            Face::Reg => self.reg.as_ref(),
            Face::Bold => self.bold.as_ref().or(self.reg.as_ref()),
            Face::Icon => self.icon.as_ref(),
        }
    }

    /// Font and glyph for a character, falling back to Segoe UI Symbol for arrows and such.
    fn pick(&self, f: Face, ch: char) -> Option<(&FontVec, GlyphId)> {
        let main = self.face(f);
        if let Some(font) = main {
            let id = font.glyph_id(ch);
            if id.0 != 0 {
                return Some((font, id));
            }
        }
        for font in &self.fallback {
            let id = font.glyph_id(ch);
            if id.0 != 0 {
                return Some((font, id));
            }
        }
        main.map(|f| (f, f.glyph_id(ch)))
    }

    /// PxScale that gives the em size `em` in this font.
    fn scale(font: &FontVec, em: f32) -> PxScale {
        let upem = font.units_per_em().unwrap_or(2048.0);
        PxScale::from(em * font.height_unscaled() / upem)
    }

    /// (ascent, descent) in px, both positive, rounded like GDI's text metrics.
    pub fn metrics(&self, s: Spec) -> (f32, f32) {
        match self.face(s.face) {
            Some(f) => {
                let sf = f.as_scaled(Self::scale(f, s.em));
                (sf.ascent().round(), (-sf.descent()).round())
            }
            None => ((s.em * 1.08).round(), (s.em * 0.25).round()),
        }
    }

    /// Height of one line of text (Tk's "linespace").
    pub fn linespace(&self, s: Spec) -> f32 {
        let (a, d) = self.metrics(s);
        a + d
    }

    pub fn measure(&self, s: Spec, text: &str) -> f32 {
        let mut w = 0.0;
        let mut prev: Option<(GlyphId, *const FontVec)> = None;
        for ch in text.chars() {
            let Some((font, id)) = self.pick(s.face, ch) else { continue };
            let sf = font.as_scaled(Self::scale(font, s.em));
            if let Some((p, pf)) = prev {
                if std::ptr::eq(pf, font) {
                    w += sf.kern(p, id);
                }
            }
            w += sf.h_advance(id);
            prev = Some((id, font as *const _));
        }
        w
    }

    /// Split text into lines no wider than `width` (at spaces, like Tk's wraplength).
    pub fn wrap(&self, s: Spec, text: &str, width: f32) -> Vec<String> {
        let mut lines = Vec::new();
        for para in text.split('\n') {
            let mut line = String::new();
            for word in para.split(' ') {
                let cand = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
                if !line.is_empty() && self.measure(s, &cand) > width {
                    lines.push(std::mem::take(&mut line));
                    line = word.to_string();
                } else {
                    line = cand;
                }
            }
            lines.push(line);
        }
        lines
    }
}

pub struct Canvas {
    pub pm: Pixmap,
}

fn paint(c: Rgb) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.0, c.1, c.2, 255);
    p.anti_alias = true;
    p
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Option<Rgb>) -> Canvas {
        let mut pm = Pixmap::new(w.max(1), h.max(1)).expect("canvas size");
        if let Some(c) = bg {
            pm.fill(Color::from_rgba8(c.0, c.1, c.2, 255));
        }
        Canvas { pm }
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb) {
        if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            let mut p = paint(c);
            p.anti_alias = false;
            self.pm.fill_rect(r, &p, Transform::identity(), None);
        }
    }

    pub fn rrect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: Rgb) {
        fill_path(&mut self.pm, rounded_rect(x0, y0, x1, y1, r), arr(c));
    }

    /// Rounded outline drawn inside the box, `width` px wide.
    #[allow(clippy::too_many_arguments)]
    pub fn rrect_outline(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: Rgb, width: f32) {
        let i = width / 2.0;
        if let Some(path) = rounded_rect(x0 + i, y0 + i, x1 - i, y1 - i, (r - i).max(0.0)) {
            let st = Stroke { width, ..Stroke::default() };
            self.pm.stroke_path(&path, &paint(c), &st, Transform::identity(), None);
        }
    }

    pub fn ellipse(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgb) {
        if let Some(r) = tiny_skia::Rect::from_ltrb(x0, y0, x1, y1) {
            if let Some(p) = PathBuilder::from_oval(r) {
                fill_path(&mut self.pm, Some(p), arr(c));
            }
        }
    }

    pub fn polyline(&mut self, pts: &[(f32, f32)], c: Rgb, width: f32) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for p in &pts[1..] {
            pb.line_to(p.0, p.1);
        }
        if let Some(path) = pb.finish() {
            let st = Stroke { width, line_cap: LineCap::Butt, line_join: LineJoin::Round, ..Stroke::default() };
            self.pm.stroke_path(&path, &paint(c), &st, Transform::identity(), None);
        }
    }

    pub fn blit(&mut self, src: &Pixmap, x: f32, y: f32) {
        self.pm.draw_pixmap(x.round() as i32, y.round() as i32, src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }

    /// Draw text with its baseline at `baseline`, starting at x. Returns the advance width.
    pub fn text(&mut self, fonts: &Fonts, s: Spec, x: f32, baseline: f32, text: &str, c: Rgb) -> f32 {
        let (w, h) = (self.pm.width() as i32, self.pm.height() as i32);
        let data = self.pm.data_mut();
        let mut pen = x;
        let mut prev: Option<(GlyphId, *const FontVec)> = None;
        for ch in text.chars() {
            let Some((font, id)) = fonts.pick(s.face, ch) else { continue };
            let sf = font.as_scaled(Fonts::scale(font, s.em));
            if let Some((p, pf)) = prev {
                if std::ptr::eq(pf, font) {
                    pen += sf.kern(p, id);
                }
            }
            let glyph = id.with_scale_and_position(sf.scale(), ab_glyph::point(pen, baseline));
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|gx, gy, cov| {
                    let px = b.min.x as i32 + gx as i32;
                    let py = b.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= w || py >= h {
                        return;
                    }
                    // A little extra weight so small text reads like Windows' own.
                    let a = cov.clamp(0.0, 1.0).powf(0.85);
                    if a <= 0.0 {
                        return;
                    }
                    let i = ((py * w + px) * 4) as usize;
                    let px_ = &mut data[i..i + 4];
                    for (k, sc) in [c.0, c.1, c.2].into_iter().enumerate() {
                        px_[k] = (sc as f32 * a + px_[k] as f32 * (1.0 - a)).round() as u8;
                    }
                    px_[3] = (255.0 * a + px_[3] as f32 * (1.0 - a)).round() as u8;
                });
            }
            pen += sf.h_advance(id);
            prev = Some((id, font as *const _));
        }
        pen - x
    }

    /// Text placed like PIL's anchors: `h` is 'l' (left) or 'm' (middle), `v` is 'm' (middle of
    /// ascender..descender) or 't' (top of the ascender).
    #[allow(clippy::too_many_arguments)]
    pub fn text_anchor(&mut self, fonts: &Fonts, s: Spec, x: f32, y: f32, anchor: (char, char), text: &str, c: Rgb) {
        let (asc, desc) = fonts.metrics(s);
        let x = if anchor.0 == 'm' { x - fonts.measure(s, text) / 2.0 } else { x };
        let baseline = if anchor.1 == 't' { y + asc } else { y + (asc - desc) / 2.0 };
        self.text(fonts, s, x, baseline, text, c);
    }
}

/// Straight BGRA bytes (for icons) from a premultiplied pixmap.
pub fn bgra_straight(pm: &Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pm.data().len());
    for p in pm.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
    }
    out
}

/// Premultiplied BGRA bytes (for layered windows) from a premultiplied pixmap.
pub fn bgra_premul(pm: &Pixmap) -> Vec<u8> {
    let mut out = pm.data().to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
}
