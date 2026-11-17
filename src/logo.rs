//! The app logo: orange rounded square with two dark blocks.
//! Shared by build.rs (the exe icon) and the app (tray, popup header, login window).

use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

pub const CLAY_RGB: [u8; 3] = [0xD9, 0x77, 0x57];
pub const DARK_RGB: [u8; 3] = [44, 18, 8];

/// Rounded rectangle from (x0, y0) to (x1, y1) with corner radius r.
pub fn rounded_rect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Option<Path> {
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + k, y0, x1, y0 + r - k, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - k, y1, x0, y1 - r + k, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - k, x0 + r - k, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}

pub fn fill_path(pm: &mut Pixmap, path: Option<Path>, c: [u8; 3]) {
    if let Some(p) = path {
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], 255);
        paint.anti_alias = true;
        pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// The logo at `size` px. `margin` is the empty border as a fraction of the size;
/// `bg` fills the background (transparent when None).
pub fn draw_logo(size: u32, margin: f32, bg: Option<[u8; 3]>) -> Pixmap {
    let mut pm = Pixmap::new(size.max(1), size.max(1)).expect("logo size");
    if let Some(c) = bg {
        pm.fill(Color::from_rgba8(c[0], c[1], c[2], 255));
    }
    let s = size as f32;
    let m = s * margin;
    fill_path(&mut pm, rounded_rect(m, m, s - m, s - m, s * 0.2), CLAY_RGB);
    let b = s * 0.2; // block size
    let g = s * 0.06; // gap between blocks
    let (x0, y0) = ((s - (2.0 * b + g)) / 2.0, (s - b) / 2.0);
    for x in [x0, x0 + b + g] {
        fill_path(&mut pm, rounded_rect(x, y0, x + b, y0 + b, s * 0.02), DARK_RGB);
    }
    pm
}

/// Straight (not premultiplied) RGBA bytes of a pixmap.
#[allow(dead_code)]
pub fn rgba(pm: &Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pm.data().len());
    for p in pm.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}
