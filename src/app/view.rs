//! Drawing: the popup pages (usage and settings), the taskbar pill and their pieces.
//!
//! Sizes, paddings and colors match the original Python version, so the app looks the same.

use super::*;
use crate::usage::{RELEASES_URL, USAGE_URL};

// ----------------------------------------------------------------- images

fn bar_image(w: u32, h: u32, pct: f64, col: Rgb) -> Pixmap {
    let mut c = Canvas::new(w, h, Some(BG));
    let r = h as f32 / 2.0;
    c.rrect(0.0, 0.0, w as f32, h as f32, r, TRACK);
    if pct > 0.0 {
        let fw = (h as f32).max(w as f32 * pct.min(100.0) as f32 / 100.0);
        c.rrect(0.0, 0.0, fw, h as f32, r, col);
    }
    c.pm
}

/// Rounded on/off switch: Claude orange when on.
fn toggle_image(s: f32, on: bool) -> Pixmap {
    let px = |v: f32| (v * s).trunc();
    let (w, h, m) = (px(34.0), px(18.0), px(3.0));
    let mut c = Canvas::new(w as u32, h as u32, Some(BG));
    c.rrect(0.0, 0.0, w, h, h / 2.0, if on { CLAY } else { TRACK });
    let k = h - 2.0 * m;
    let x = if on { w - k - m } else { m };
    c.ellipse(x, m, x + k, m + k, if on { WHITE } else { FG2 });
    c.pm
}

/// Rounded pill used for choices (refresh interval, thresholds, source, tabs).
fn chip_image(f: &Fonts, s: f32, text: &str, on: bool) -> Pixmap {
    let px = |v: f32| (v * s).trunc();
    let (h, pad) = (px(22.0), px(10.0));
    let spec = Spec { face: if on { Face::Bold } else { Face::Reg }, em: px(11.0) };
    let w = (f.measure(spec, text) + 2.0 * pad).trunc();
    let mut c = Canvas::new(w as u32, h as u32, Some(BG));
    if on {
        c.rrect(0.0, 0.0, w, h, h / 2.0, CLAY);
    } else {
        c.rrect(0.0, 0.0, w, h, h / 2.0, BG2);
        c.rrect_outline(0.0, 0.0, w, h, h / 2.0, TRACK, 1.0);
    }
    c.text_anchor(f, spec, w / 2.0, h / 2.0, ('m', 'm'), text, if on { DARK } else { FG2 });
    c.pm
}

/// One part of the taskbar pill: (label, pct_text, timer_text, color).
pub(super) type Seg = (String, String, String, Rgb);

/// Taskbar pill. segs: (label, pct_text, timer_text, color); label is "" when only one is shown.
pub(super) fn pill(fonts: &Fonts, scale: f32, square: bool, outline: bool, bg: Rgb, segs: &[Seg]) -> Pixmap {
    let px = |v: f32| (v * scale).trunc();
    let h = px(26.0);
    let (pad, gap, small, sep) = (px(8.0), px(6.0), px(4.0), px(14.0));
    let bold = Spec { face: Face::Bold, em: px(12.0) };
    let reg = Spec { face: Face::Reg, em: px(12.0) };
    let col = segs.iter().map(|s| s.3).max_by_key(|c| severity(*c)).unwrap_or(MUTED);
    let (sq_w, sq_h) = if square { (px(12.0), px(10.0)) } else { (0.0, 0.0) };
    enum Part {
        Sep(f32),
        Txt(f32, String, Spec, Rgb),
    }
    let mut parts = Vec::new();
    let mut x = pad + sq_w + if sq_w > 0.0 { gap } else { 0.0 };
    for (i, (label, text, timer, c)) in segs.iter().enumerate() {
        if i > 0 {
            parts.push(Part::Sep(x + sep / 2.0));
            x += sep;
        }
        if !label.is_empty() {
            parts.push(Part::Txt(x, label.clone(), reg, MUTED));
            x += fonts.measure(reg, label) + small;
        }
        parts.push(Part::Txt(x, text.clone(), bold, *c));
        x += fonts.measure(bold, text);
        if !timer.is_empty() {
            x += gap;
            parts.push(Part::Txt(x, timer.clone(), reg, MUTED));
            x += fonts.measure(reg, timer);
        }
    }
    let w = (x + pad).trunc();
    let mut c = Canvas::new(w as u32, h as u32, Some(bg));
    let lw = scale.round().max(1.0);
    if outline {
        c.rrect_outline(0.0, 0.0, w, h, px(6.0), col, lw);
    }
    if sq_w > 0.0 {
        let sy = (h - sq_h) / 2.0;
        c.rrect(pad, sy, pad + sq_w, sy + sq_h, px(2.0), col);
    }
    for p in parts {
        match p {
            Part::Sep(sx) => c.rect(sx - lw / 2.0, h * 0.3, lw, h * 0.4, MUTED),
            Part::Txt(tx, t, sp, color) => c.text_anchor(fonts, sp, tx, h / 2.0, ('l', 'm'), &t, color),
        }
    }
    c.pm
}

// ----------------------------------------------------------------- page layout

/// Lays out a page top to bottom: `y` is the next free row.
struct L<'a> {
    f: &'a Fonts,
    s: f32,
    x0: f32,
    cw: f32,
    y: f32,
    ops: Vec<Op>,
    hits: Vec<Hit>,
    hover_n: usize,
}

impl<'a> L<'a> {
    fn new(f: &'a Fonts, s: f32) -> L<'a> {
        let px = |v: f32| (v * s).trunc();
        L { f, s, x0: 1.0 + px(16.0), cw: px(280.0) + 2.0 * IMG, y: 1.0 + px(14.0), ops: Vec::new(), hits: Vec::new(), hover_n: 0 }
    }

    fn finish(self) -> Page {
        let w = (self.x0 * 2.0 + self.cw).round() as u32;
        let h = (self.y + self.px(14.0) + 1.0).round() as u32;
        Page { w, h, ops: self.ops, hits: self.hits }
    }

    fn px(&self, v: f32) -> f32 {
        (v * self.s).trunc()
    }

    /// Font size in points.
    fn pt(&self, pt: f32, face: Face) -> Spec {
        Spec { face, em: (pt * 96.0 / 72.0 * self.s).round() }
    }

    fn lh(&self, sp: Spec) -> f32 {
        self.f.linespace(sp) + 2.0 * INSET
    }

    fn lw(&self, sp: Spec, t: &str) -> f32 {
        self.f.measure(sp, t).ceil() + 2.0 * INSET
    }

    fn hover_id(&mut self) -> usize {
        self.hover_n += 1;
        self.hover_n
    }

    fn hit(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, action: Action, hover: Option<usize>) {
        self.hits.push(Hit { r: (x0, y0, x1, y1), action, hover, ibeam: false });
    }

    /// A text label with its box at (x, y). Returns (width, height) of the box.
    fn text(&mut self, x: f32, y: f32, t: &str, sp: Spec, c: Rgb, hover: Option<(usize, Rgb)>) -> (f32, f32) {
        let (asc, _) = self.f.metrics(sp);
        self.ops.push(Op::Text { x: x + INSET, baseline: y + INSET + asc, text: t.into(), spec: sp, color: c, hover });
        (self.lw(sp, t), self.lh(sp))
    }

    fn pix(&mut self, x: f32, y: f32, pm: Pixmap) -> (f32, f32) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        self.ops.push(Op::Pix { x, y, pm });
        (w, h)
    }

    /// A label on its own row, at the left.
    fn label(&mut self, t: &str, sp: Spec, c: Rgb, pady: (f32, f32)) {
        self.y += pady.0;
        let (_, h) = self.text(self.x0, self.y, t, sp, c, None);
        self.y += h + pady.1;
    }

    /// A label that wraps at `width` px.
    fn wrapped(&mut self, t: &str, sp: Spec, c: Rgb, width: f32, pady: (f32, f32)) {
        self.y += pady.0 + INSET;
        let (asc, _) = self.f.metrics(sp);
        for line in self.f.wrap(sp, t, width) {
            self.ops.push(Op::Text { x: self.x0 + INSET, baseline: self.y + asc, text: line, spec: sp, color: c, hover: None });
            self.y += self.f.linespace(sp);
        }
        self.y += INSET + pady.1;
    }

    /// Row with a label at the left and one at the right, vertically centered.
    fn row_lr(&mut self, left: (&str, Spec, Rgb), right: Option<(&str, Spec, Rgb)>) -> f32 {
        let h = self.lh(left.1).max(right.map(|r| self.lh(r.1)).unwrap_or(0.0));
        let y = self.y;
        self.text(self.x0, y + (h - self.lh(left.1)) / 2.0, left.0, left.1, left.2, None);
        if let Some((t, sp, c)) = right {
            let w = self.lw(sp, t);
            self.text(self.x0 + self.cw - w, y + (h - self.lh(sp)) / 2.0, t, sp, c, None);
        }
        self.y += h;
        h
    }

    fn separator(&mut self, pady: (f32, f32)) {
        self.y += pady.0;
        self.ops.push(Op::Rect { x: self.x0, y: self.y, w: self.cw, h: 1.0, c: TRACK });
        self.y += 1.0 + pady.1;
    }

    fn header(&mut self, title: &str, sub: &str, buttons: &[(char, Option<Action>)]) {
        let logo = self.px(30.0);
        let logo_box = logo + 2.0 * INSET;
        let ts = self.pt(11.0, Face::Bold);
        let ss = self.pt(8.0, Face::Reg);
        let is = self.pt(11.0, Face::Icon);
        let th = self.lh(ts) + self.lh(ss);
        let bh = self.lh(is);
        let h = logo_box.max(th).max(bh);
        let y0 = self.y;
        self.pix(self.x0 + INSET, y0 + (h - logo_box) / 2.0 + INSET, draw_logo(logo as u32, 0.0, Some([BG.0, BG.1, BG.2])));
        let tx = self.x0 + logo_box + self.px(10.0);
        let ty = y0 + (h - th) / 2.0;
        self.text(tx, ty, title, ts, FG, None);
        self.text(tx, ty + self.lh(ts), sub, ss, MUTED, None);
        // Packed side=right in reverse order, so the last button ends up rightmost.
        let mut x = self.x0 + self.cw;
        for (sym, act) in buttons.iter().rev() {
            let t = sym.to_string();
            let bw = self.lw(is, &t);
            x -= bw;
            let by = y0 + (h - bh) / 2.0;
            match act {
                None => {
                    self.text(x, by, &t, is, TRACK, None);
                }
                Some(a) => {
                    let id = self.hover_id();
                    self.text(x, by, &t, is, MUTED, Some((id, CLAY)));
                    self.hit(x, by, x + bw, by + bh, a.clone(), Some(id));
                }
            }
            x -= self.px(10.0);
        }
        self.y += h + self.px(12.0);
    }

    fn bar_block(&mut self, label: &str, pct: f64, reset: &Option<DateTime<Utc>>, note: &str) {
        let r9 = self.pt(9.0, Face::Reg);
        let b9 = self.pt(9.0, Face::Bold);
        let r8 = self.pt(8.0, Face::Reg);
        let used = format!("{pct:.0}% used");
        self.row_lr((label, r9, FG2), Some((&used, b9, FG)));
        self.y += self.px(4.0);
        let (w, h) = (self.px(280.0), self.px(8.0));
        self.pix(self.x0 + IMG, self.y + IMG, bar_image(w as u32, h as u32, pct, color_for(pct)));
        self.y += h + 2.0 * IMG + self.px(3.0);
        let mut txt = fmt_reset(reset);
        if 100.0 - pct <= 20.0 {
            txt = format!("{:.0}% left · {txt}", 100.0 - pct);
        }
        self.label(&txt, r8, MUTED, (0.0, 0.0));
        if !note.is_empty() {
            self.label(note, r8, MUTED, (0.0, 0.0));
        }
        self.y += self.px(10.0);
    }

    /// A credits line: label and value, an optional sub line; the whole row can be a link.
    fn credit_line(&mut self, label: &str, value: &str, value_fg: Rgb, sub: &str, sub_fg: Rgb, url: Option<&str>) {
        let y0 = self.y;
        self.row_lr((label, self.pt(9.0, Face::Reg), FG2), Some((value, self.pt(9.0, Face::Bold), value_fg)));
        if !sub.is_empty() {
            let r8 = self.pt(8.0, Face::Reg);
            self.label(sub, r8, sub_fg, (0.0, 0.0));
        }
        if let Some(u) = url {
            self.hit(self.x0, y0, self.x0 + self.cw, self.y, Action::Url(u.into()), None);
        }
        self.y += self.px(6.0);
    }

    fn chips_row(&mut self, chips: Vec<(Pixmap, Action)>) {
        let mut x = self.x0;
        let mut h: f32 = 0.0;
        for (pm, a) in chips {
            let (w, ph) = (pm.width() as f32 + 2.0 * IMG, pm.height() as f32 + 2.0 * IMG);
            self.pix(x + IMG, self.y + IMG, pm);
            self.hit(x, self.y, x + w, self.y + ph, a, None);
            x += w + self.px(4.0);
            h = h.max(ph);
        }
        self.y += h;
    }

    fn section(&mut self, text: &str) {
        let sp = self.pt(7.0, Face::Bold);
        let (a, b) = (self.px(8.0), self.px(2.0));
        self.label(&text.to_uppercase(), sp, MUTED, (a, b));
    }

    /// Title (+ description) at the left, a control at the right. Returns the control's
    /// top-left so the caller can place it; reserves `ctrl` size.
    fn row(&mut self, title: &str, desc: &str, ctrl: (f32, f32)) -> (f32, f32) {
        let r9 = self.pt(9.0, Face::Reg);
        let r8 = self.pt(8.0, Face::Reg);
        self.y += self.px(4.0);
        let y0 = self.y;
        let wrap_w = self.px(210.0);
        let lines = if desc.is_empty() { Vec::new() } else { self.f.wrap(r8, desc, wrap_w) };
        let lh = self.lh(r9) + if lines.is_empty() { 0.0 } else { lines.len() as f32 * self.f.linespace(r8) + 2.0 * INSET };
        let h = lh.max(ctrl.1);
        let mut y = y0 + (h - lh) / 2.0;
        let (_, th) = self.text(self.x0, y, title, r9, FG, None);
        y += th;
        if !lines.is_empty() {
            let (asc, _) = self.f.metrics(r8);
            y += INSET;
            for line in lines {
                self.ops.push(Op::Text { x: self.x0 + INSET, baseline: y + asc, text: line, spec: r8, color: MUTED, hover: None });
                y += self.f.linespace(r8);
            }
        }
        self.y = y0 + h + self.px(4.0);
        (self.x0 + self.cw - ctrl.0, y0 + (h - ctrl.1) / 2.0)
    }

    fn toggle(&mut self, key: &'static str, on: bool, title: &str, desc: &str) {
        let pm = toggle_image(self.s, on);
        let size = (pm.width() as f32 + 2.0 * IMG, pm.height() as f32 + 2.0 * IMG);
        let (x, y) = self.row(title, desc, size);
        self.pix(x + IMG, y + IMG, pm);
        self.hit(x, y, x + size.0, y + size.1, Action::Toggle(key), None);
    }

    /// Optional title above a row of chips.
    fn labeled(&mut self, title: &str, chips: Vec<(Pixmap, Action)>) {
        self.y += self.px(4.0);
        if !title.is_empty() {
            let r9 = self.pt(9.0, Face::Reg);
            let b = self.px(4.0);
            self.label(title, r9, FG, (0.0, b));
        }
        self.chips_row(chips);
        self.y += self.px(4.0);
    }
}

impl App {
    pub(super) fn paint(&self) -> Option<Pixmap> {
        let page = self.page.as_ref()?;
        let mut c = Canvas::new(page.w, page.h, Some(TRACK));
        c.rect(1.0, 1.0, page.w as f32 - 2.0, page.h as f32 - 2.0, BG);
        for op in &page.ops {
            match op {
                Op::Text { x, baseline, text, spec, color, hover } => {
                    let col = match hover {
                        Some((id, hc)) if self.hover == Some(*id) => *hc,
                        _ => *color,
                    };
                    c.text(&self.fonts, *spec, *x, *baseline, text, col);
                }
                Op::Pix { x, y, pm } => c.blit(pm, *x, *y),
                Op::Rect { x, y, w, h, c: col } => c.rect(*x, *y, *w, *h, *col),
            }
        }
        Some(c.pm)
    }

    // ---------------- main page

    pub(super) fn layout_main(&self) -> Page {
        let mut l = L::new(&self.fonts, self.scale);
        let mut sub = match self.updated {
            Some(t) => {
                let mins = ((now() - t) / 60.0) as i64;
                if mins < 1 { "Updated just now".to_string() } else { format!("Updated {mins} min ago") }
            }
            None => "Loading…".into(),
        };
        if !self.plan.is_empty() {
            sub += &format!(" · {}", self.plan);
        }
        let locked = self.manual_refresh_wait() > 0;
        l.header("Claude usage", &sub, &[(ICON_REFRESH, if locked { None } else { Some(Action::Refresh) }), (ICON_SETTINGS, Some(Action::Settings))]);
        if let Some(v) = &self.new_version {
            self.update_banner(&mut l, v);
        }
        if let Some(usage) = &self.usage {
            for u in usage {
                l.bar_block(&u.label, u.pct, &u.resets_at, &u.note);
            }
            if self.settings.b("show_credits") {
                self.credits_section(&mut l);
            }
            if self.settings.b("show_chart") {
                self.history_chart(&mut l);
            }
        }
        let r8 = l.pt(8.0, Face::Reg);
        let top = l.px(8.0);
        if let Some(e) = &self.error {
            let w = l.px(280.0);
            l.wrapped(e, r8, AMBER, w, (top, 0.0));
        } else if !self.source.is_empty() {
            l.label(&format!("Source: {}", self.source), r8, MUTED, (top, 0.0));
        }
        l.finish()
    }

    /// Clickable 'new version available' strip that opens the GitHub release page.
    fn update_banner(&self, l: &mut L, version: &str) {
        let b9 = l.pt(9.0, Face::Bold);
        let r9 = l.pt(9.0, Face::Reg);
        let (padx, pady) = (l.px(10.0), l.px(6.0));
        let h = 2.0 + l.lh(b9) + 2.0 * pady;
        let (x0, y0, w) = (l.x0, l.y, l.cw);
        l.ops.push(Op::Rect { x: x0, y: y0, w, h, c: CLAY });
        l.ops.push(Op::Rect { x: x0 + 1.0, y: y0 + 1.0, w: w - 2.0, h: h - 2.0, c: BG2 });
        l.text(x0 + 1.0 + padx, y0 + 1.0 + pady, &format!("New version {version} available"), b9, FG, None);
        let t = "Press here  ↗";
        let tw = l.lw(r9, t);
        l.text(x0 + w - 1.0 - padx - tw, y0 + (h - l.lh(r9)) / 2.0, t, r9, CLAY, None);
        l.hit(x0, y0, x0 + w, y0 + h, Action::Url(RELEASES_URL.into()), None);
        l.y += h + l.px(12.0);
    }

    /// Usage credits: what's left, monthly spend limit, promo credits and when they expire.
    fn credits_section(&self, l: &mut L) {
        let Some(c) = &self.credits else { return };
        let r9 = l.pt(9.0, Face::Reg);
        let r8 = l.pt(8.0, Face::Reg);
        l.y += l.px(2.0);
        let y0 = l.y;
        let link = "Open usage page  ↗";
        let h = l.row_lr(("Credits", r9, FG2), Some((link, r8, CLAY)));
        let lw = l.lw(r8, link);
        let x1 = l.x0 + l.cw;
        l.hit(x1 - lw, y0, x1, y0 + h, Action::Url(USAGE_URL.into()), None);
        l.y += l.px(4.0);

        let mut shown = false;
        if let Some(b) = &c.balance {
            let (s, col) = expiry(&c.balance_expires);
            l.credit_line("Credits left", &fmt_money(b), FG, &s, col, None);
            shown = true;
        }
        if let Some(p) = &c.promo {
            let (s, col) = expiry(&c.promo_expires);
            l.credit_line("Promo credits", &fmt_money(p), FG, &s, col, None);
            shown = true;
        }
        if c.enabled && c.limit.is_some() {
            let limit = c.limit.clone().unwrap_or(Money { amount: 0.0, currency: "USD".into() });
            let used = c.used.clone().unwrap_or(Money { amount: 0.0, currency: limit.currency.clone() });
            let cur = used.currency.clone();
            let pct = if limit.amount != 0.0 { 100.0 * used.amount / limit.amount } else { 0.0 };
            let left = Money { amount: (limit.amount - used.amount).max(0.0), currency: cur.clone() };
            l.row_lr(("Monthly credit limit", r9, FG2), Some((&format!("{} left", fmt_money(&left)), l.pt(9.0, Face::Bold), FG)));
            l.y += l.px(4.0);
            let (w, bh) = (l.px(280.0), l.px(8.0));
            l.pix(l.x0 + IMG, l.y + IMG, bar_image(w as u32, bh as u32, pct, color_for(pct)));
            l.y += bh + 2.0 * IMG + l.px(3.0);
            let lim_cur = Money { amount: limit.amount, currency: cur.clone() };
            let (txt, fg) = if c.limit_reached {
                ("Monthly limit reached".to_string(), RED)
            } else {
                (format!("{} of {} used this month", fmt_money(&used), fmt_money(&lim_cur)), MUTED)
            };
            l.label(&txt, r8, fg, (0.0, 0.0));
            l.y += l.px(6.0);
            shown = true;
        } else if c.enabled && c.used.is_some() {
            l.credit_line("Credits used", &fmt_money(c.used.as_ref().unwrap()), FG, "", MUTED, None);
            shown = true;
        }
        if !shown {
            l.credit_line("Usage credits", if c.enabled { "On" } else { "Off" }, if c.enabled { FG } else { MUTED }, "", MUTED, None);
        }
        // Resets, offers and promos: shown even when usage credits are off.
        let extras: Vec<_> = c.extras.iter().filter(|x| x.key != "promo_credits" || c.promo.is_none()).collect();
        if extras.is_empty() {
            l.credit_line("Resets & extras", "None available", MUTED, "", MUTED, None);
        }
        for x in extras {
            let value = if let Some(a) = &x.amount {
                fmt_money(a)
            } else if let Some(n) = &x.count {
                format!("{} available", usage::value_str(n))
            } else {
                "Available".into()
            };
            let (s, col) = expiry(&x.expires);
            l.credit_line(&x.name, &value, CLAY, &s, col, Some(USAGE_URL));
        }
    }

    /// This week's usage: one bar per day of the current weekly period (up to the weekly reset).
    fn history_chart(&self, l: &mut L) {
        let wk = self.usage.as_ref().and_then(|u| u.iter().find(|x| x.key == "seven_day"));
        let end = wk.and_then(|w| w.resets_at).unwrap_or_else(|| Utc::now() + Duration::days(1));
        let start = end - Duration::days(7);
        let r9 = l.pt(9.0, Face::Reg);
        let (a, b) = (l.px(4.0), l.px(4.0));
        l.label(&format!("This week · until {}", local(&end).format("%a %H:%M")), r9, FG2, (a, b));
        let (w, h, lbl) = (l.px(280.0), l.px(70.0), l.px(16.0));
        // One bar per calendar day from the week's start day to its reset day.
        let first = local(&start).date_naive();
        let last = local(&end).date_naive();
        let n = ((last - first).num_days() + 1).max(1) as usize;
        let s0 = start.timestamp() as f64;
        let span = 7.0 * 86400.0;
        let mut peak = vec![0.0f64; n];
        let mut line: Vec<(f32, f64)> = Vec::new();
        for smp in &self.history {
            if s0 <= smp.t && smp.t < s0 + span {
                let Some(dt) = Local.timestamp_opt(smp.t as i64, 0).single() else { continue };
                let i = (dt.date_naive() - first).num_days();
                if 0 <= i && (i as usize) < n {
                    peak[i as usize] = peak[i as usize].max(smp.fh);
                }
                if let Some(sd) = smp.sd {
                    // x position by the time of day, so the line lines up with the day bars
                    use chrono::Timelike;
                    let frac = (dt.hour() * 60 + dt.minute()) as f32 / 1440.0;
                    line.push(((i as f32 + frac) / n as f32, sd));
                }
            }
        }
        let now_i = (Local::now().date_naive() - first).num_days();
        let mut c = Canvas::new(w as u32, (h + lbl) as u32, Some(BG));
        for y in [0.0, h / 2.0, h] {
            // gridlines at 100%, 50%, 0%
            c.rect(0.0, y.round().min(h - 1.0), w, 1.0, TRACK);
        }
        let slot = w / n as f32;
        let bw = slot * 0.55;
        let lf = Spec { face: Face::Reg, em: l.px(9.0) };
        for (i, v) in peak.iter().enumerate() {
            let cx = (i as f32 + 0.5) * slot;
            if *v > 0.0 {
                let top = h - l.px(3.0).max(h * v.min(100.0) as f32 / 100.0);
                c.rrect(cx - bw / 2.0, top, cx + bw / 2.0, h, l.px(3.0), color_for(*v));
            }
            let day = first + Duration::days(i as i64);
            let col = if i as i64 == now_i { FG } else { MUTED };
            c.text_anchor(l.f, lf, cx, h + l.px(3.0), ('m', 't'), &day.format("%a").to_string(), col);
        }
        if line.len() > 1 {
            // weekly % used over the week
            let pts: Vec<(f32, f32)> = line.iter().map(|(x, y)| (x * w, h - h * y.min(100.0) as f32 / 100.0)).collect();
            c.polyline(&pts, FG2, (1.5 * self.scale).trunc().max(1.0));
        }
        l.pix(l.x0 + IMG, l.y + IMG, c.pm);
        l.y += h + lbl + 2.0 * IMG;
        // legend
        let r8 = l.pt(8.0, Face::Reg);
        l.y += l.px(2.0);
        let mut x = l.x0;
        for (t, col) in [("■", CLAY), ("Daily peak, 5-hour   ", MUTED), ("—", FG2), ("Weekly used", MUTED)] {
            let (tw, _) = l.text(x, l.y, t, r8, col, None);
            x += tw;
        }
        l.y += l.lh(r8);
    }

    // ---------------- settings page

    pub(super) fn layout_settings(&self) -> Page {
        let mut l = L::new(&self.fonts, self.scale);
        let s = &self.settings;
        let (f, sc) = (&self.fonts, self.scale);
        l.header("Settings", "Changes save automatically", &[(ICON_BACK, Some(Action::Main))]);
        let chip = |t: &str, on: bool| chip_image(f, sc, t, on);
        let choices = |key: &'static str, opts: &[(Value, &str)]| -> Vec<(Pixmap, Action)> {
            opts.iter().map(|(v, t)| (chip(t, s.get(key) == v), Action::Set(key, v.clone()))).collect()
        };
        let multi = |items: &[(&'static str, &str)]| -> Vec<(Pixmap, Action)> {
            items.iter().map(|(k, t)| (chip(t, s.b(k)), Action::Toggle(k))).collect()
        };
        match self.tab {
            "general" => {
                l.section("Startup");
                l.toggle("autostart", s.b("autostart"), "Start with Windows", "");
                l.toggle("check_updates", s.b("check_updates"), "Check for updates", "Banner when a new version is out");
                let c = choices("refresh_minutes", &[(json!(5), "5 min"), (json!(10), "10 min"), (json!(15), "15 min"), (json!(30), "30 min")]);
                l.labeled("Refresh every", c);
            }
            "display" => {
                l.section("Taskbar");
                l.toggle("show_taskbar_widget", s.b("show_taskbar_widget"), "Usage next to the clock", "");
                if s.b("show_taskbar_widget") {
                    l.labeled("", multi(&[("widget_five", "5-hour"), ("widget_weekly", "Weekly"), ("widget_timer", "Time left")]));
                    l.labeled("", multi(&[("widget_square", "Square"), ("widget_outline", "Outline")]));
                }
                let c = choices("tray_show", &[(json!("off"), "Off"), (json!("five_hour"), "5-hour"), (json!("seven_day"), "Weekly")]);
                l.labeled("Number in the tray icon", c);
                l.section("Popup");
                l.toggle("show_credits", s.b("show_credits"), "Credits & resets", "With the link to the usage page");
                l.toggle("show_chart", s.b("show_chart"), "This week's graph", "");
            }
            "alerts" => {
                l.section("Notifications");
                l.labeled("When this much is left", multi(&[("notify_20", "20%"), ("notify_10", "10%"), ("notify_5", "5%")]));
                l.toggle("notify_weekly", s.b("notify_weekly"), "Include weekly limits", "Weekly and per-model limits, like Fable");
                let types: Vec<(String, String)> = match &self.usage {
                    Some(u) => u.iter().map(|x| (x.key.clone(), x.label.clone())).collect(),
                    None => vec![("five_hour".into(), "5-hour session".into()), ("seven_day".into(), "Weekly, all models".into())],
                };
                let c = types
                    .iter()
                    .map(|(k, lab)| {
                        let on = self.reset_enabled(k);
                        (chip(&short_name(k, lab), on), Action::ResetKey(k.clone(), !on))
                    })
                    .collect();
                l.labeled("When a limit resets", c);
                let c = choices("notify_reset_left", &[(json!(100), "Any"), (json!(50), "50%"), (json!(20), "20%"), (json!(10), "10%")]);
                l.labeled("Only if this much was left", c);
            }
            _ => {
                l.section("Data source");
                let c = choices("source", &[(json!("auto"), "Auto"), (json!("claude_code"), "Claude Code"), (json!("claude_ai"), "claude.ai")]);
                l.labeled("", c);
                l.toggle("allow_token_refresh", s.b("allow_token_refresh"), "Renew Claude Code login", "When it has expired");
                l.section("claude.ai");
                let logged_in = !s.s("session_key").is_empty() && !self.entry_open;
                let (title, desc) = if logged_in {
                    ("Logged in", "Resets and credits are shown")
                } else {
                    ("Not logged in", "Log in to see limit resets and credits")
                };
                let r9 = l.pt(9.0, Face::Reg);
                if self.login.is_some() {
                    let t = "Logging in…";
                    let size = (l.lw(r9, t), l.lh(r9));
                    let (x, y) = l.row(title, desc, size);
                    l.text(x, y, t, r9, MUTED, None);
                } else if logged_in || cfg!(feature = "login") {
                    let pm = chip(if logged_in { "Log out" } else { "Log in" }, !logged_in);
                    let size = (pm.width() as f32 + 2.0 * IMG, pm.height() as f32 + 2.0 * IMG);
                    let (x, y) = l.row(title, desc, size);
                    l.pix(x + IMG, y + IMG, pm);
                    l.hit(x, y, x + size.0, y + size.1, if logged_in { Action::Logout } else { Action::Login }, None);
                    if !logged_in {
                        let r8 = l.pt(8.0, Face::Reg);
                        let (a, b) = (l.px(6.0), l.px(4.0));
                        l.label("Or paste the sessionKey cookie", r8, FG2, (a, b));
                        self.entry(&mut l);
                    }
                } else {
                    // No login window in this build: only the pasted cookie.
                    l.row(title, desc, (0.0, 0.0));
                    let r8 = l.pt(8.0, Face::Reg);
                    let (a, b) = (l.px(6.0), l.px(4.0));
                    l.label("Paste the sessionKey cookie", r8, FG2, (a, b));
                    self.entry(&mut l);
                }
            }
        }
        let w = l.px(280.0);
        let (a, b) = (l.px(12.0), l.px(8.0));
        l.separator((a, b));
        let r9 = l.pt(9.0, Face::Reg);
        let r8 = l.pt(8.0, Face::Reg);
        let fh = l.lh(r9).max(l.lh(r8));
        let y = l.y;
        let id = l.hover_id();
        let q = "Quit Claude Usage";
        let (qw, qh) = l.text(l.x0, y + (fh - l.lh(r9)) / 2.0, q, r9, MUTED, Some((id, RED)));
        l.hit(l.x0, y, l.x0 + qw, y + qh, Action::Quit, Some(id));
        let v = format!("Version {VERSION}");
        let vw = l.lw(r8, &v);
        l.text(l.x0 + l.cw - vw, y + (fh - l.lh(r8)) / 2.0, &v, r8, MUTED, None);
        l.y += fh;
        if self.tab == "general" {
            let r7 = l.pt(7.0, Face::Reg);
            let top = l.px(8.0);
            l.wrapped("Unofficial app, not affiliated with or endorsed by Anthropic. Claude is a trademark of Anthropic, PBC.", r7, MUTED, w, (top, 0.0));
        }
        // Tabs last: the popup is anchored at the bottom, so they stay put when switching tabs.
        let (a, b) = (l.px(10.0), l.px(8.0));
        l.separator((a, b));
        let tabs: Vec<(Pixmap, Action)> = [("general", "General"), ("display", "Display"), ("alerts", "Alerts"), ("account", "Account")]
            .iter()
            .map(|(k, t)| (chip(t, self.tab == *k), Action::Tab(k)))
            .collect();
        l.chips_row(tabs);
        l.finish()
    }

    /// The sessionKey field: dots instead of the key, orange outline while typing.
    fn entry(&self, l: &mut L) {
        let r9 = l.pt(9.0, Face::Reg);
        let (padx, pady) = (l.px(8.0), l.px(5.0));
        let th = self.fonts.linespace(r9) + 2.0;
        let h = 2.0 + 2.0 * pady + th;
        // "Paste" button at the right: right-click does nothing in this custom field.
        let paste = chip_image(&self.fonts, self.scale, "Paste", false);
        let (pw, ph) = (paste.width() as f32 + 2.0 * IMG, paste.height() as f32 + 2.0 * IMG);
        let (x0, y0) = (l.x0, l.y);
        let w = l.cw - pw - l.px(6.0);
        let (bx, by) = (x0 + l.cw - pw, y0 + (h - ph) / 2.0);
        l.pix(bx + IMG, by + IMG, paste);
        l.hit(bx, by, bx + pw, by + ph, Action::Paste, None);
        l.ops.push(Op::Rect { x: x0, y: y0, w, h, c: if self.entry_focus { CLAY } else { TRACK } });
        l.ops.push(Op::Rect { x: x0 + 1.0, y: y0 + 1.0, w: w - 2.0, h: h - 2.0, c: BG2 });
        let room = w - 2.0 - 2.0 * padx - 2.0;
        let mut dots: String = "•".repeat(self.entry_text.chars().count());
        while !dots.is_empty() && self.fonts.measure(r9, &dots) > room {
            dots.pop();
        }
        let (asc, _) = self.fonts.metrics(r9);
        let tx = x0 + 1.0 + padx;
        let base = y0 + 1.0 + pady + 1.0 + asc;
        l.ops.push(Op::Text { x: tx, baseline: base, text: dots.clone(), spec: r9, color: FG, hover: None });
        if self.entry_focus {
            let cx = tx + self.fonts.measure(r9, &dots) + 1.0;
            l.ops.push(Op::Rect { x: cx, y: y0 + 1.0 + pady + 1.0, w: 1.0, h: th - 2.0, c: FG });
        }
        l.hits.push(Hit { r: (x0, y0, x0 + w, y0 + h), action: Action::Entry, hover: None, ibeam: true });
        l.y += h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the README's widget.png: `CU_SHOTS_DIR=assets/screenshots cargo test -- --ignored widget_screenshot`
    #[test]
    #[ignore]
    fn widget_screenshot() {
        let dir = std::env::var("CU_SHOTS_DIR").expect("set CU_SHOTS_DIR");
        let f = Fonts::load();
        let seg = |l: &str, p: &str, t: &str, c| (l.to_string(), p.to_string(), t.to_string(), c);
        let variants: Vec<Vec<Seg>> = vec![
            vec![seg("", "42%", "3h 56m", CLAY)],
            vec![seg("5h", "42%", "3h 56m", CLAY), seg("W", "63%", "1d 21h", CLAY)],
            vec![seg("", "63%", "", CLAY)],
            vec![seg("", "85%", "3h 56m", AMBER)],
            vec![seg("", "97%", "3h 56m", RED)],
        ];
        let bg = (32, 32, 32);
        let pills: Vec<Pixmap> = variants.iter().map(|v| pill(&f, 1.0, true, true, bg, v)).collect();
        let (margin, gap) = (12.0, 10.0);
        let w = pills.iter().map(|p| p.width()).max().unwrap() as f32 + 2.0 * margin;
        let h = pills.iter().map(|p| p.height() as f32).sum::<f32>() + 2.0 * margin + gap * (pills.len() - 1) as f32;
        let mut c = Canvas::new(w as u32, h as u32, Some(bg));
        let mut y = margin;
        for p in &pills {
            c.blit(p, margin, y);
            y += p.height() as f32 + gap;
        }
        c.pm.save_png(std::path::Path::new(&dir).join("widget.png")).unwrap();
    }
}
