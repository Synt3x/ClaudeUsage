//! The app: taskbar widget, popup (usage and settings pages), tray icon, notifications.
//!
//! Every page is laid out like the Python/Tk version (same sizes, paddings and colors) and
//! drawn into a layered window, so a page change swaps size and pixels in one step.

use crate::draw::*;
use crate::logo::draw_logo;
use crate::store::{self, log, now, truthy, Settings, MIN_FETCH_GAP, VERSION};
use crate::usage::{self, Credits, FetchError, Limit, Money, Reading, RELEASES_URL, RESET_FIELDS, USAGE_URL};
use crate::win::{self, wide};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::ptr::null_mut;
use std::sync::mpsc::{channel, Receiver, Sender};
use tiny_skia::Pixmap;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_TRAY: u32 = WM_APP + 1;
const WM_WORKER: u32 = WM_APP + 2;
const T_TICK: usize = 1;
const T_TOP: usize = 2;
const T_CLICK: usize = 3;
const T_SHOW: usize = 4;
const T_UNLOCK: usize = 5;

const ICON_REFRESH: char = '\u{E72C}';
const ICON_SETTINGS: char = '\u{E713}';
const ICON_BACK: char = '\u{E72B}';

/// Tk label border + padding, in pixels (not scaled, like Tk).
const INSET: f32 = 2.0;
/// Padding of an image label with bd=0.
const IMG: f32 = 1.0;

enum Msg {
    Result(Result<Reading, FetchError>),
    NewVersion(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Refresh,
    Settings,
    Main,
    Url(String),
    Toggle(&'static str),
    Set(&'static str, Value),
    ResetKey(String, bool),
    Tab(&'static str),
    Login,
    Logout,
    Quit,
    Entry,
}

enum Op {
    Text { x: f32, baseline: f32, text: String, spec: Spec, color: Rgb, hover: Option<(usize, Rgb)> },
    Pix { x: f32, y: f32, pm: Pixmap },
    Rect { x: f32, y: f32, w: f32, h: f32, c: Rgb },
}

struct Hit {
    r: (f32, f32, f32, f32),
    action: Action,
    hover: Option<usize>,
    ibeam: bool,
}

struct Page {
    w: u32,
    h: u32,
    ops: Vec<Op>,
    hits: Vec<Hit>,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Main,
    Settings,
}

struct Sample {
    t: f64,
    fh: f64,
    sd: Option<f64>,
}

// ----------------------------------------------------------------- helpers

fn color_for(pct_used: f64) -> Rgb {
    let left = 100.0 - pct_used;
    if left <= 5.0 {
        RED
    } else if left <= 20.0 {
        AMBER
    } else {
        CLAY
    }
}

fn severity(c: Rgb) -> u8 {
    match c {
        RED => 3,
        AMBER => 2,
        CLAY => 1,
        _ => 0,
    }
}

fn local(t: &DateTime<Utc>) -> DateTime<Local> {
    t.with_timezone(&Local)
}

fn fmt_reset(dt: &Option<DateTime<Utc>>) -> String {
    let Some(dt) = dt else { return String::new() };
    let secs = (*dt - Utc::now()).num_seconds().max(0);
    if secs < 24 * 3600 {
        let (h, m) = (secs / 60 / 60, secs / 60 % 60);
        format!("Resets in {h} h {m} min ({})", local(dt).format("%H:%M"))
    } else {
        format!("Resets {}", local(dt).format("%a %H:%M"))
    }
}

/// Short chip name for a limit: "5-hour", "Weekly", "Fable".
fn short_name(key: &str, label: &str) -> String {
    match key {
        "five_hour" => "5-hour".into(),
        "seven_day" => "Weekly".into(),
        _ => match label.split_once(',') {
            Some((_, rest)) => rest.trim().into(),
            None => label.into(),
        },
    }
}

/// Name used in notifications: "5-hour limit", "weekly limit", "weekly Fable limit".
fn limit_name(key: &str, label: &str) -> String {
    match key {
        "five_hour" => "5-hour limit".into(),
        "seven_day" => "weekly limit".into(),
        _ => format!("weekly {} limit", short_name(key, label)),
    }
}

/// Compact time until dt: "45m", "3h 57m", "2d 5h".
fn short_left(dt: &Option<DateTime<Utc>>) -> String {
    let Some(dt) = dt else { return String::new() };
    let mins = ((*dt - Utc::now()).num_seconds() / 60).max(0);
    if mins >= 24 * 60 {
        format!("{}d {}h", mins / 1440, mins % 1440 / 60)
    } else if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

fn fmt_money(m: &Money) -> String {
    let neg = m.amount < 0.0;
    let s = format!("{:.2}", m.amount.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let num = format!("{}{grouped}.{frac}", if neg { "-" } else { "" });
    match m.currency.to_uppercase().as_str() {
        "USD" => format!("${num}"),
        "EUR" => format!("€{num}"),
        "GBP" => format!("£{num}"),
        _ => format!("{num} {}", m.currency),
    }
}

fn expiry(dt: &Option<DateTime<Utc>>) -> (String, Rgb) {
    let Some(dt) = dt else { return (String::new(), MUTED) };
    let days = (*dt - Utc::now()).num_milliseconds() as f64 / 86_400_000.0;
    let l = local(dt);
    if days < 0.0 {
        return ("Expired".into(), RED);
    }
    let when = if days < 1.0 {
        format!("today {}", l.format("%H:%M"))
    } else {
        format!("{} (in {} days)", l.format("%a %d %b"), days as i64)
    };
    (format!("Expires {when}"), if days <= 7.0 { AMBER } else { MUTED })
}

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

// ----------------------------------------------------------------- page layout

/// Lays out a page top to bottom like Tk's pack(): `y` is the next free row.
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

    /// Tk font in points.
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

// ----------------------------------------------------------------- tray icon

struct Tray {
    hicon: HICON,
    sig: Option<(Vec<u8>, String)>,
}

fn copy_w(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

// ----------------------------------------------------------------- app

pub struct App {
    settings: Settings,
    history: Vec<Sample>,
    notified: Map<String, Value>,
    usage: Option<Vec<Limit>>,
    credits: Option<Credits>,
    plan: String,
    source: String,
    error: Option<String>,
    updated: Option<f64>,
    fetching: bool,
    next_fetch: f64,
    last_fetch: f64,
    rate_until: f64,
    rate_strikes: u32,
    popup_opened_at: f64,
    popup_had_focus: bool,
    popup_hidden_at: f64,
    new_version: Option<String>,
    view: View,
    tab: &'static str,
    scale: f32,
    fonts: Fonts,
    root: HWND,
    widget: HWND,
    popup: HWND,
    page: Option<Page>,
    popup_pos: (i32, i32),
    hover: Option<usize>,
    covered: bool,
    owner: HWND,
    widget_pm: Option<Pixmap>,
    widget_at: Option<(i32, i32)>,
    widget_dirty: bool,
    tray: Tray,
    hook: HHOOK,
    login: Option<std::process::Child>,
    entry_focus: bool,
    entry_text: String,
    entry_open: bool,
    last_minute: i64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// (popup, widget, widget shown) for the mouse hook, which can't borrow the app.
    static HOOK_INFO: Cell<(isize, isize, bool)> = const { Cell::new((0, 0, false)) };
    static OUTSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Low-level mouse hook, only while the popup is open: a press outside it closes it.
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == 0 && matches!(wp as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        let (p, w, shown) = HOOK_INFO.get();
        let (x, y) = (info.pt.x, info.pt.y);
        let inside = win::point_in(p as HWND, x, y);
        let on_widget = shown && win::point_in(w as HWND, x, y);
        if !inside && !on_widget && !win::menu_open() {
            OUTSIDE.set(true);
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// Work that must run without the app borrowed (it pumps messages).
enum After {
    Nothing,
    Menu { x: i32, y: i32, tray: bool },
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let res = APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(|app| app.handle(h, msg, wp, lp)),
        Err(_) => None, // re-entered (menu loop, focus change): let Windows handle it
    });
    match res {
        Some((Some(r), after)) => {
            run_after(after);
            r
        }
        Some((None, after)) => {
            run_after(after);
            DefWindowProcW(h, msg, wp, lp)
        }
        None => DefWindowProcW(h, msg, wp, lp),
    }
}

fn run_after(after: After) {
    if let After::Menu { x, y, tray } = after {
        let Some(root) = APP.with(|a| a.try_borrow().ok().and_then(|g| g.as_ref().map(|a| a.root as isize))) else { return };
        let items: Vec<Option<&str>> = if tray {
            vec![Some("Open"), Some("Refresh now"), Some("Settings"), Some("Quit")]
        } else {
            vec![Some("Open"), Some("Refresh now"), Some("Settings"), None, Some("Quit")]
        };
        let cmd = win::popup_menu(root as HWND, &items, x, y, if tray { Some(1) } else { None });
        let label = if cmd > 0 { items[cmd as usize - 1] } else { None };
        if let Some(label) = label {
            APP.with(|a| {
                if let Ok(mut g) = a.try_borrow_mut() {
                    if let Some(app) = g.as_mut() {
                        match label {
                            "Open" => app.toggle_popup(),
                            "Refresh now" => app.manual_refresh(),
                            "Settings" => app.show_popup(View::Settings),
                            "Quit" => app.quit(),
                            _ => {}
                        }
                    }
                }
            });
        }
    }
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as u16 as i16 as i32, ((lp >> 16) & 0xFFFF) as u16 as i16 as i32)
}

fn create_window(class: &[u16], ex: u32, style: u32) -> HWND {
    unsafe {
        CreateWindowExW(ex, class.as_ptr(), class.as_ptr(), style, 0, 0, 0, 0, null_mut(), null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null())
    }
}

pub fn run() {
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    win::dark_menus();
    let class = wide("ClaudeUsageWnd");
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
    }
    let root = create_window(&class, WS_EX_TOOLWINDOW, WS_POPUP);
    let widget = create_window(&class, WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, WS_POPUP);
    let popup = create_window(&class, WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, WS_POPUP);

    let app = App::new(root, widget, popup);
    APP.with(|a| *a.borrow_mut() = Some(app));
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.start();
        }
    });
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().take() {
            app.cleanup();
        }
    });
}

impl App {
    fn new(root: HWND, widget: HWND, popup: HWND) -> App {
        let settings = Settings::load();
        if settings.b("autostart") {
            // keep the startup entry pointing at whatever is running now
            let _ = store::set_autostart(true);
        }
        let mut history: Vec<Sample> = store::load_json(&store::path("history.json"))
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|h| {
                Some(Sample { t: h.get("t")?.as_f64()?, fh: h.get("fh").and_then(|v| v.as_f64()).unwrap_or(0.0), sd: h.get("sd").and_then(|v| v.as_f64()) })
            })
            .collect();
        if store::demo() {
            let mut seed = now() as u64 | 1;
            let mut rnd = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let t = now();
            history = (1..=30)
                .rev()
                .filter(|d| d % 5 != 0)
                .map(|d| Sample {
                    t: (t - d as f64 * 86400.0).trunc(),
                    fh: [0.0, 20.0, 45.0, 70.0, 90.0, 100.0][(rnd() % 6) as usize],
                    sd: Some(((30 - d) % 7 * 12) as f64),
                })
                .collect();
        }
        let notified = match store::load_json(&store::path("notified.json")) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let (tx, rx) = channel();
        let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        App {
            settings,
            history,
            notified,
            usage: None,
            credits: None,
            plan: String::new(),
            source: String::new(),
            error: None,
            updated: None,
            fetching: false,
            next_fetch: 0.0,
            last_fetch: 0.0,
            rate_until: 0.0,
            rate_strikes: 0,
            popup_opened_at: 0.0,
            popup_had_focus: false,
            popup_hidden_at: 0.0,
            new_version: None,
            view: View::Main,
            tab: "general",
            scale: win::system_scale(),
            fonts: Fonts::load(),
            root,
            widget,
            popup,
            page: None,
            popup_pos: (0, 0),
            hover: None,
            covered: false,
            owner: null_mut(),
            widget_pm: None,
            widget_at: None,
            widget_dirty: true,
            tray: Tray { hicon: null_mut(), sig: None },
            hook: null_mut(),
            login: None,
            entry_focus: false,
            entry_text: String::new(),
            entry_open: false,
            last_minute: 0,
            tx,
            rx,
            taskbar_created,
        }
    }

    fn start(&mut self) {
        self.add_tray();
        if !store::demo() {
            if let Some((r, t)) = usage::load_last_usage() {
                self.usage = Some(r.usage);
                self.plan = r.plan;
                self.source = r.source;
                self.credits = r.credits;
                self.updated = Some(t);
            }
        }
        self.update_widget();
        let st = store::load_fetch_state();
        let f = |k: &str| st.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        self.last_fetch = f("last_fetch");
        self.rate_until = f("rate_until");
        usage::set_ai_cache(f("ai_t"), st.get("ai_data").and_then(usage::AiData::from_json));
        // After a restart, wait until the regular interval since the last request has passed.
        self.next_fetch = (self.last_fetch + self.refresh_secs()).max(self.rate_until);
        if now() >= self.next_fetch {
            self.request_fetch();
        }
        if self.settings.b("check_updates") {
            self.check_for_update();
        }
        unsafe {
            SetTimer(self.root, T_TICK, 1000, None);
            SetTimer(self.root, T_TOP, 300, None);
            SetTimer(self.root, T_CLICK, 50, None);
            if store::has_arg("--show") {
                SetTimer(self.root, T_SHOW, 1500, None);
            }
        }
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale) as i32
    }

    fn refresh_secs(&self) -> f64 {
        self.settings.i("refresh_minutes") as f64 * 60.0
    }

    fn popup_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.popup) != 0 }
    }

    // ---------------- messages

    fn handle(&mut self, h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> (Option<LRESULT>, After) {
        let none = (None, After::Nothing);
        let done = (Some(0), After::Nothing);
        if h == self.root {
            match msg {
                WM_TIMER => {
                    match wp {
                        T_TICK => self.tick(),
                        T_TOP => self.keep_on_top(),
                        T_CLICK => self.watch_outside_click(),
                        T_SHOW => {
                            unsafe { KillTimer(self.root, T_SHOW) };
                            let v = if store::has_arg("--settings") { View::Settings } else { View::Main };
                            self.show_popup(v);
                        }
                        T_UNLOCK => {
                            unsafe { KillTimer(self.root, T_UNLOCK) };
                            if self.popup_visible() && self.view == View::Main {
                                self.render_popup();
                            }
                        }
                        _ => {}
                    }
                    return done;
                }
                WM_WORKER => {
                    while let Ok(m) = self.rx.try_recv() {
                        match m {
                            Msg::Result(r) => self.on_result(r),
                            Msg::NewVersion(v) => {
                                self.new_version = Some(v);
                                if self.popup_visible() && self.view == View::Main {
                                    self.render_popup();
                                }
                            }
                        }
                    }
                    return done;
                }
                WM_TRAY => {
                    match (lp & 0xFFFF) as u32 {
                        WM_LBUTTONUP => self.toggle_popup(),
                        WM_RBUTTONUP => {
                            let (x, y) = win::cursor_pos();
                            return (Some(0), After::Menu { x, y, tray: true });
                        }
                        _ => {}
                    }
                    return done;
                }
                m if m == self.taskbar_created && m != 0 => {
                    // Explorer restarted: the tray icon has to be added again.
                    self.tray.sig = None;
                    self.add_tray();
                    self.update_tray();
                    return done;
                }
                _ => return none,
            }
        }
        if h == self.widget {
            match msg {
                WM_LBUTTONUP => {
                    self.toggle_popup();
                    return done;
                }
                WM_RBUTTONUP => {
                    let (x, y) = win::cursor_pos();
                    return (Some(0), After::Menu { x, y, tray: false });
                }
                WM_SETCURSOR => {
                    unsafe { SetCursor(LoadCursorW(null_mut(), IDC_ARROW)) };
                    return (Some(1), After::Nothing);
                }
                WM_MOUSEACTIVATE => return (Some(MA_NOACTIVATE as isize), After::Nothing),
                _ => return none,
            }
        }
        if h == self.popup {
            match msg {
                WM_LBUTTONDOWN => {
                    let (x, y) = lparam_xy(lp);
                    self.popup_click(x as f32, y as f32);
                    return done;
                }
                WM_MOUSEMOVE => {
                    let (x, y) = lparam_xy(lp);
                    let hv = self.hit_at(x as f32, y as f32).and_then(|i| self.page.as_ref().and_then(|p| p.hits[i].hover));
                    if hv != self.hover {
                        self.hover = hv;
                        self.repaint_popup();
                    }
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.popup, dwHoverTime: 0 };
                    unsafe { TrackMouseEvent(&mut tme) };
                    return done;
                }
                WM_MOUSELEAVE => {
                    if self.hover.is_some() {
                        self.hover = None;
                        self.repaint_popup();
                    }
                    return done;
                }
                WM_SETCURSOR => {
                    let (sx, sy) = win::cursor_pos();
                    let (x, y) = ((sx - self.popup_pos.0) as f32, (sy - self.popup_pos.1) as f32);
                    let cur = match self.hit_at(x, y).and_then(|i| self.page.as_ref().map(|p| &p.hits[i])) {
                        Some(hit) if hit.ibeam => IDC_IBEAM,
                        Some(_) => IDC_HAND,
                        None => IDC_ARROW,
                    };
                    unsafe { SetCursor(LoadCursorW(null_mut(), cur)) };
                    return (Some(1), After::Nothing);
                }
                WM_KEYDOWN => {
                    if wp as u16 == VK_ESCAPE {
                        self.hide_popup();
                    }
                    return done;
                }
                WM_CHAR => {
                    if self.entry_focus {
                        self.entry_key(wp as u32);
                    }
                    return done;
                }
                _ => return none,
            }
        }
        none
    }

    // ---------------- taskbar widget

    fn taskbar_bg(&self) -> Rgb {
        // Sample the taskbar color so the pill blends in (light and dark taskbars).
        match win::taskbar_anchor(self.scale) {
            Some((right, top, _)) => win::screen_pixel(right + 2, top + 3).unwrap_or((32, 32, 32)),
            None => (32, 32, 32),
        }
    }

    /// Taskbar pill. segs: (label, pct_text, timer_text, color); label is "" when only one is shown.
    fn pill_image(&self, segs: &[(String, String, String, Rgb)]) -> Pixmap {
        let bg = self.taskbar_bg();
        let px = |v: f32| (v * self.scale).trunc();
        let h = px(26.0);
        let (pad, gap, small, sep) = (px(8.0), px(6.0), px(4.0), px(14.0));
        let bold = Spec { face: Face::Bold, em: px(12.0) };
        let reg = Spec { face: Face::Reg, em: px(12.0) };
        let st = &self.settings;
        let col = segs.iter().map(|s| s.3).max_by_key(|c| severity(*c)).unwrap_or(MUTED);
        let (sq_w, sq_h) = if st.b("widget_square") { (px(12.0), px(10.0)) } else { (0.0, 0.0) };
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
                x += self.fonts.measure(reg, label) + small;
            }
            parts.push(Part::Txt(x, text.clone(), bold, *c));
            x += self.fonts.measure(bold, text);
            if !timer.is_empty() {
                x += gap;
                parts.push(Part::Txt(x, timer.clone(), reg, MUTED));
                x += self.fonts.measure(reg, timer);
            }
        }
        let w = (x + pad).trunc();
        let mut c = Canvas::new(w as u32, h as u32, Some(bg));
        let lw = self.scale.round().max(1.0);
        if st.b("widget_outline") {
            c.rrect_outline(0.0, 0.0, w, h, px(6.0), col, lw);
        }
        if sq_w > 0.0 {
            let sy = (h - sq_h) / 2.0;
            c.rrect(pad, sy, pad + sq_w, sy + sq_h, px(2.0), col);
        }
        for p in parts {
            match p {
                Part::Sep(sx) => c.rect(sx - lw / 2.0, h * 0.3, lw, h * 0.4, MUTED),
                Part::Txt(tx, t, sp, color) => c.text_anchor(&self.fonts, sp, tx, h / 2.0, ('l', 'm'), &t, color),
            }
        }
        c.pm
    }

    fn update_widget(&mut self) {
        let st = &self.settings;
        let segs: Vec<(String, String, String, Rgb)> = match &self.usage {
            Some(usage) => {
                let mut keys: Vec<&str> = [("five_hour", st.b("widget_five")), ("seven_day", st.b("widget_weekly"))]
                    .into_iter()
                    .filter(|(k, on)| *on && usage.iter().any(|l| l.key == *k))
                    .map(|(k, _)| k)
                    .collect();
                if keys.is_empty() {
                    keys.push("five_hour");
                }
                let both = keys.len() > 1;
                keys.iter()
                    .filter_map(|k| usage.iter().find(|l| l.key == *k))
                    .map(|u| {
                        let timer = if st.b("widget_timer") { short_left(&u.resets_at) } else { String::new() };
                        let label = if both { if u.key == "five_hour" { "5h" } else { "W" } } else { "" };
                        (label.to_string(), format!("{:.0}%", u.pct), timer, color_for(u.pct))
                    })
                    .collect()
            }
            None => vec![(String::new(), if self.error.is_some() { "!" } else { "--%" }.into(), String::new(), MUTED)],
        };
        self.widget_pm = Some(self.pill_image(&segs));
        self.widget_dirty = true;
        self.update_tray();
        self.place_widget();
    }

    fn place_widget(&mut self) {
        if !self.settings.b("show_taskbar_widget") {
            unsafe { ShowWindow(self.widget, SW_HIDE) };
            return;
        }
        if self.covered {
            return; // hidden by keep_on_top() over a fullscreen app
        }
        let Some(pm) = &self.widget_pm else { return };
        let (ww, wh) = (pm.width() as i32, pm.height() as i32);
        let tray = win::tray_wnd();
        let pos = match win::taskbar_anchor(self.scale) {
            Some((right, top, height)) if !tray.is_null() => {
                // Make the taskbar our owner: Windows always keeps an owned window above its
                // owner, so clicking the taskbar can't cover the widget.
                if self.owner != tray {
                    unsafe { SetWindowLongPtrW(self.widget, GWLP_HWNDPARENT, tray as isize) };
                    self.owner = tray;
                }
                (right - ww - self.px(6.0), top + (height - wh) / 2)
            }
            _ => {
                let wa = win::work_area();
                (wa.right - ww - self.px(320.0) + self.settings.i("widget_offset") as i32, wa.bottom - wh - self.px(10.0))
            }
        };
        // Only update when needed; re-placing it every second made it flicker.
        if self.widget_dirty || self.widget_at != Some(pos) {
            win::update_layered(self.widget, pm, pos.0, pos.1);
            self.widget_dirty = false;
            self.widget_at = Some(pos);
        }
        unsafe {
            if IsWindowVisible(self.widget) == 0 {
                ShowWindow(self.widget, SW_SHOWNOACTIVATE);
            }
        }
        HOOK_INFO.set((self.popup as isize, self.widget as isize, true));
    }

    /// Put the widget back above the taskbar if something (e.g. the snipping tool) pushed it under.
    /// The widget is topmost, so it would also float over fullscreen apps such as remote
    /// desktop: hide it while the taskbar itself is covered or auto-hidden.
    fn keep_on_top(&mut self) {
        if !self.settings.b("show_taskbar_widget") {
            return;
        }
        let covered = win::taskbar_covered(&[self.widget, self.popup, self.root]);
        if covered != self.covered {
            self.covered = covered;
            unsafe { ShowWindow(self.widget, if covered { SW_HIDE } else { SW_SHOWNOACTIVATE }) };
        }
        if !covered {
            unsafe {
                SetWindowPos(self.widget, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
            }
        }
    }

    // ---------------- tray icon

    fn tray_image(&self) -> Pixmap {
        let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
        let k = n as f32 / 64.0;
        let key = self.settings.s("tray_show");
        match self.usage.as_ref().and_then(|u| u.iter().find(|l| l.key == key)) {
            Some(l) => {
                let col = color_for(l.pct);
                let mut c = Canvas::new(n, n, None);
                c.rrect(0.0, 0.0, n as f32, n as f32, 12.0 * k, col);
                let text = format!("{:.0}", l.pct.min(99.0));
                let em = if text.chars().count() < 2 { 40.0 } else { 36.0 } * k;
                c.text_anchor(&self.fonts, Spec { face: Face::Bold, em }, n as f32 / 2.0, n as f32 / 2.0, ('m', 'm'), &text, INK);
                c.pm
            }
            None => draw_logo(n, 0.03, None),
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.root;
        nid.uID = 1;
        nid
    }

    fn add_tray(&mut self) {
        let pm = self.tray_image();
        let icon = win::make_icon(&pm);
        let mut nid = self.nid();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        copy_w(&mut nid.szTip, "Claude usage");
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
        if !self.tray.hicon.is_null() {
            unsafe { DestroyIcon(self.tray.hicon) };
        }
        self.tray.hicon = icon;
    }

    /// Tray icon number and tooltip, following the "Number in the tray icon" setting.
    fn update_tray(&mut self) {
        let pm = self.tray_image();
        let key = self.settings.s("tray_show");
        let title = match &self.usage {
            Some(usage) => {
                let lines: Vec<String> = usage
                    .iter()
                    .filter(|l| l.key == "five_hour" || l.key == "seven_day")
                    .map(|l| format!("{}: {:.0}% used", l.label, l.pct))
                    .collect();
                let mut t = format!("Claude usage\n{}", lines.join("\n"));
                if let Some(l) = usage.iter().find(|l| l.key == key) {
                    t += &format!("\n{}", fmt_reset(&l.resets_at));
                }
                t
            }
            None => "Claude usage".into(),
        };
        let sig = (pm.data().to_vec(), title.clone());
        if self.tray.sig.as_ref() == Some(&sig) {
            return; // only touch the icon when it changed
        }
        self.tray.sig = Some(sig);
        let icon = win::make_icon(&pm);
        let mut nid = self.nid();
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = icon;
        copy_w(&mut nid.szTip, &title); // Windows tooltip limit: 127 chars
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        if !self.tray.hicon.is_null() {
            unsafe { DestroyIcon(self.tray.hicon) };
        }
        self.tray.hicon = icon;
    }

    fn notify(&self, title: &str, msg: &str) {
        let mut nid = self.nid();
        nid.uFlags = NIF_INFO;
        copy_w(&mut nid.szInfo, msg);
        copy_w(&mut nid.szInfoTitle, title);
        nid.dwInfoFlags = NIIF_NONE;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    // ---------------- popup

    fn render_popup(&mut self) {
        let page = match self.view {
            View::Main => self.layout_main(),
            View::Settings => self.layout_settings(),
        };
        self.page = Some(page);
        self.popup_opened_at = now(); // clicks that started this page change aren't "outside"
        self.position_popup();
    }

    fn paint(&self) -> Option<Pixmap> {
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

    fn repaint_popup(&mut self) {
        if let Some(pm) = self.paint() {
            win::update_layered(self.popup, &pm, self.popup_pos.0, self.popup_pos.1);
        }
    }

    fn position_popup(&mut self) {
        let Some(page) = &self.page else { return };
        let (pw, ph) = (page.w as i32, page.h as i32);
        let wa = win::work_area();
        let mut x = if self.settings.b("show_taskbar_widget") {
            win::rect_of(self.widget).right - pw + self.px(40.0)
        } else {
            wa.right - pw - self.px(12.0)
        };
        x = x.min(wa.right - pw - self.px(8.0)).max(wa.left + self.px(8.0));
        let y = wa.bottom - ph - self.px(12.0);
        self.popup_pos = (x, y);
        self.repaint_popup();
    }

    fn show_popup(&mut self, view: View) {
        self.view = view;
        self.hover = None;
        self.render_popup();
        unsafe {
            ShowWindow(self.popup, SW_SHOW);
            SetForegroundWindow(self.popup);
        }
        self.popup_opened_at = now();
        self.popup_had_focus = false;
        HOOK_INFO.set((self.popup as isize, self.widget as isize, self.settings.b("show_taskbar_widget")));
        self.mouse_hook(true);
    }

    fn hide_popup(&mut self) {
        unsafe { ShowWindow(self.popup, SW_HIDE) };
        self.popup_hidden_at = now();
        self.entry_focus = false;
        self.entry_open = false;
        self.entry_text.clear();
        self.mouse_hook(false);
    }

    fn toggle_popup(&mut self) {
        if self.popup_visible() {
            self.hide_popup();
        } else if now() - self.popup_hidden_at > 0.3 {
            // (a click on the tray icon first closes the popup as an outside click)
            self.show_popup(View::Main);
        }
    }

    fn mouse_hook(&mut self, on: bool) {
        unsafe {
            if on && self.hook.is_null() {
                OUTSIDE.set(false);
                self.hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), GetModuleHandleW(std::ptr::null()), 0);
            } else if !on && !self.hook.is_null() {
                UnhookWindowsHookEx(self.hook);
                self.hook = null_mut();
            }
        }
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<usize> {
        let page = self.page.as_ref()?;
        page.hits.iter().position(|h| h.r.0 <= x && x < h.r.2 && h.r.1 <= y && y < h.r.3)
    }

    fn popup_click(&mut self, x: f32, y: f32) {
        let action = self.hit_at(x, y).and_then(|i| self.page.as_ref().map(|p| p.hits[i].action.clone()));
        let was_focus = self.entry_focus;
        self.entry_focus = action == Some(Action::Entry);
        let Some(action) = action else {
            if was_focus {
                self.render_popup();
            }
            return;
        };
        match action {
            Action::Refresh => self.manual_refresh(),
            Action::Settings => {
                self.view = View::Settings;
                self.render_popup();
            }
            Action::Main => {
                self.view = View::Main;
                self.render_popup();
            }
            Action::Url(u) => win::open_url(&u),
            Action::Toggle(k) => {
                let v = !self.settings.b(k);
                self.set_setting(k, json!(v));
            }
            Action::Set(k, v) => self.set_setting(k, v),
            Action::ResetKey(k, v) => {
                let mut keys = self.settings.get("notify_reset_keys").as_object().cloned().unwrap_or_default();
                keys.insert(k, json!(v));
                self.set_setting("notify_reset_keys", Value::Object(keys));
            }
            Action::Tab(t) => {
                self.tab = t;
                self.entry_open = false;
                self.render_popup();
            }
            Action::Login => self.start_login(),
            Action::Logout => self.logout_claude_ai(),
            Action::Quit => self.quit(),
            Action::Entry => {
                self.entry_open = true;
                self.render_popup();
            }
        }
    }

    fn entry_key(&mut self, ch: u32) {
        match ch {
            8 => {
                self.entry_text.pop();
            }
            0x16 => {
                // Ctrl+V
                if let Some(t) = win::clipboard_text(self.popup) {
                    self.entry_text.extend(t.chars().filter(|c| !c.is_control()));
                }
            }
            0x01 | 0x18 => self.entry_text.clear(), // Ctrl+A / Ctrl+X: start over
            c if c >= 0x20 => {
                if let Some(c) = char::from_u32(c) {
                    self.entry_text.push(c);
                }
            }
            _ => return,
        }
        self.set_session_key(&self.entry_text.clone());
        self.render_popup();
    }

    // ---------------- main page

    fn layout_main(&self) -> Page {
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

    fn layout_settings(&self) -> Page {
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
        let (x0, y0, w) = (l.x0, l.y, l.cw);
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

    fn set_setting(&mut self, key: &str, value: Value) {
        let old = self.settings.get(key).clone();
        self.settings.set(key, value.clone());
        self.settings.save();
        if key == "autostart" && value != old {
            if let Err(e) = store::set_autostart(truthy(&value)) {
                log(&format!("Couldn't change autostart: {e}"));
            }
        }
        if key == "show_taskbar_widget" {
            self.place_widget();
        }
        if key.starts_with("widget_") || key == "tray_show" {
            self.update_widget();
        }
        if key == "check_updates" && truthy(&value) && !truthy(&old) {
            self.check_for_update();
        }
        if key == "source" || key == "allow_token_refresh" {
            self.next_fetch = now() + 1.0;
        }
        self.render_popup();
    }

    fn reset_enabled(&self, key: &str) -> bool {
        match self.settings.get("notify_reset_keys").get(key) {
            Some(v) => truthy(v),
            None => self.settings.b("notify_reset"),
        }
    }

    /// Open a claude.ai window (separate process); it hands back the session key after login.
    fn start_login(&mut self) {
        let result = store::path("login_result.json");
        let _ = std::fs::remove_file(&result);
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                log(&format!("Couldn't open the login window: {e}"));
                return;
            }
        };
        match std::process::Command::new(exe).arg("--login").spawn() {
            Ok(child) => self.login = Some(child),
            Err(e) => {
                log(&format!("Couldn't open the login window: {e}"));
                return;
            }
        }
        self.hide_popup();
    }

    fn check_login(&mut self) {
        let Some(child) = self.login.as_mut() else { return };
        if !matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return;
        }
        self.login = None;
        let result = store::path("login_result.json");
        let key = store::load_json(&result).and_then(|v| v.get("key").and_then(|k| k.as_str()).map(|s| s.to_string())).unwrap_or_default();
        let _ = std::fs::remove_file(&result);
        if !key.is_empty() {
            self.settings.set("session_key", json!(key));
            self.settings.save();
            usage::reset_ai_cache_time(); // ask claude.ai on the next refresh (at most 5 minutes away)
            self.next_fetch = now() + 1.0;
            self.notify("Logged in to claude.ai", "Limit resets and credits will show in Claude Usage.");
            log("Logged in to claude.ai");
        }
        if self.popup_visible() && self.view == View::Settings {
            self.render_popup();
        }
    }

    fn logout_claude_ai(&mut self) {
        self.settings.set("session_key", json!(""));
        self.settings.save();
        usage::set_ai_cache(0.0, None);
        store::save_fetch_state(&[("ai_t", json!(0)), ("ai_data", Value::Null)]);
        if let Some(c) = self.credits.as_mut() {
            c.extras.retain(|x| !RESET_FIELDS.contains(&x.key.split(':').next().unwrap_or("")));
        }
        self.render_popup();
    }

    fn set_session_key(&mut self, value: &str) {
        self.settings.set("session_key", json!(value.trim()));
        self.settings.save();
        self.next_fetch = now() + 2.0; // wait until typing stops
    }

    // ---------------- loop

    fn tick(&mut self) {
        self.place_widget();
        if self.popup_visible() && now() - self.popup_opened_at > 0.6 {
            let fg = unsafe { GetForegroundWindow() };
            if fg == self.popup || win::menu_open() {
                self.popup_had_focus = true;
            } else if self.popup_had_focus {
                self.hide_popup(); // user clicked somewhere else
            }
        }
        if now() >= self.next_fetch {
            self.request_fetch();
        }
        let minute = (now() / 60.0) as i64;
        if minute != self.last_minute {
            // keep the countdown current
            self.last_minute = minute;
            self.update_widget();
        }
        self.check_login();
    }

    /// Close the popup when the mouse is pressed anywhere outside it. The focus check in
    /// tick() alone isn't enough: Windows often won't give the popup focus.
    fn watch_outside_click(&mut self) {
        // Read every time, even while closed, so a click from before the popup opened is
        // cleared instead of closing it right away.
        let pressed = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].iter().any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } as u16 & 0x8001 != 0);
        let outside = OUTSIDE.replace(false);
        if !self.popup_visible() || now() - self.popup_opened_at <= 0.3 {
            return;
        }
        if outside {
            self.hide_popup();
            return;
        }
        if pressed {
            // fallback if the hook isn't available
            let (x, y) = win::cursor_pos();
            let inside = win::point_in(self.popup, x, y);
            let on_widget = self.settings.b("show_taskbar_widget") && win::point_in(self.widget, x, y);
            if !inside && !on_widget && !win::menu_open() {
                self.hide_popup();
            }
        }
    }

    /// Seconds left before the refresh button can be used again (0 = available).
    fn manual_refresh_wait(&self) -> i64 {
        let left = (self.last_fetch + MIN_FETCH_GAP - now()).max(self.rate_until - now());
        if left > 0.0 { left as i64 + 1 } else { 0 }
    }

    fn manual_refresh(&mut self) {
        if self.manual_refresh_wait() > 0 {
            return;
        }
        self.request_fetch();
        unsafe { SetTimer(self.root, T_UNLOCK, (MIN_FETCH_GAP * 1000.0) as u32 + 200, None) };
        if self.popup_visible() && self.view == View::Main {
            self.render_popup();
        }
    }

    fn post(&self) -> impl Fn(Msg) + Send + 'static {
        let tx = self.tx.clone();
        let root = self.root as usize;
        move |m| {
            if tx.send(m).is_ok() {
                unsafe { PostMessageW(root as HWND, WM_WORKER, 0, 0) };
            }
        }
    }

    fn request_fetch(&mut self) {
        let t = now();
        if t < self.rate_until || self.fetching {
            return; // the server asked us to wait; next_fetch already points past that
        }
        let earliest = self.last_fetch + MIN_FETCH_GAP;
        if t < earliest {
            // asked too soon (reset time, settings change...): do it when allowed
            self.next_fetch = earliest;
            return;
        }
        self.last_fetch = t;
        store::save_fetch_state(&[("last_fetch", json!(t))]);
        self.next_fetch = t + self.refresh_secs();
        self.fetching = true;
        let settings = self.settings.clone();
        let post = self.post();
        std::thread::spawn(move || post(Msg::Result(usage::fetch(&settings))));
    }

    fn check_for_update(&self) {
        if store::demo() {
            return;
        }
        let post = self.post();
        std::thread::spawn(move || match usage::latest_release_version() {
            Ok(Some(latest)) if usage::version_tuple(&latest) > usage::version_tuple(VERSION) => post(Msg::NewVersion(latest)),
            Ok(_) => {}
            Err(e) => log(&format!("Update check failed: {e}")),
        });
    }

    fn on_result(&mut self, r: Result<Reading, FetchError>) {
        self.fetching = false;
        match r {
            Err(FetchError::Rate(secs)) => {
                // Back off: the server's wait time, doubled for each rate limit in a row (max 10 min).
                self.rate_strikes += 1;
                let wait = (600.0f64).min((secs.max(60) as f64) * 2f64.powi(self.rate_strikes as i32 - 1));
                self.rate_until = now() + wait;
                self.next_fetch = self.rate_until;
                store::save_fetch_state(&[("rate_until", json!(self.rate_until))]);
                let when = Local.timestamp_opt(self.next_fetch as i64, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
                let shown = if self.usage.is_some() { "Showing the last numbers; t" } else { "T" };
                self.error = Some(format!("Too many usage requests right now. {shown}rying again at {when}."));
            }
            Err(FetchError::Msg(m)) => self.error = Some(m),
            Ok(r) => {
                self.error = None;
                self.rate_strikes = 0;
                if !store::demo() {
                    usage::save_last_usage(&r);
                }
                self.usage = Some(r.usage);
                self.plan = r.plan;
                self.source = r.source;
                self.credits = r.credits;
                self.updated = Some(now());
                self.record_history();
                self.check_notifications();
                // fetch shortly after any reset so the number drops right away
                for u in self.usage.iter().flatten() {
                    if let Some(at) = u.resets_at {
                        let secs = (at - Utc::now()).num_milliseconds() as f64 / 1000.0 + 20.0;
                        if secs > 0.0 && now() + secs < self.next_fetch {
                            self.next_fetch = now() + secs;
                        }
                    }
                }
            }
        }
        self.update_widget(); // also updates the tray icon
        if self.popup_visible() && self.view == View::Main {
            self.render_popup();
        }
    }

    fn record_history(&mut self) {
        let Some(usage) = &self.usage else { return };
        let fh = usage.iter().find(|l| l.key == "five_hour").map(|l| l.pct).unwrap_or(0.0);
        let sd = usage.iter().find(|l| l.key == "seven_day").map(|l| l.pct);
        self.history.push(Sample { t: now().trunc(), fh, sd });
        let cutoff = now() - 31.0 * 86400.0;
        self.history.retain(|h| h.t >= cutoff);
        if !store::demo() {
            let v: Vec<Value> = self.history.iter().map(|h| json!({"t": h.t as i64, "fh": h.fh, "sd": h.sd})).collect();
            store::save_json(&store::path("history.json"), &Value::Array(v));
        }
    }

    /// Low-usage alerts fire once per threshold (20, 10, 5% left) and are only re-armed after a
    /// reset. resets_at jitters by a second between fetches, so it can't be used as a period id.
    fn check_notifications(&mut self) {
        let Some(usage) = self.usage.clone() else { return };
        let thresholds: Vec<i64> = [20, 10, 5].into_iter().filter(|t| self.settings.b(&format!("notify_{t}"))).collect();
        let n = Utc::now();
        for u in &usage {
            let (pct, reset) = (u.pct, u.resets_at);
            let st = self.notified.get(&u.key).cloned().unwrap_or(Value::Null);
            let mut done: BTreeSet<i64> = st.get("done").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
            let prev_pct = st.get("pct").and_then(|p| p.as_f64());
            let prev_reset = st.get("reset").and_then(usage::parse_time);
            // Did this limit reset since the last reading? Usage dropped, and either the old
            // reset time has passed or the new one is clearly later.
            let was_reset = match prev_pct {
                Some(pp) if pct < pp - 1.0 => match (prev_reset, reset) {
                    (Some(pr), Some(r)) => n >= pr - Duration::minutes(1) || r > pr + Duration::hours(1),
                    _ => true,
                },
                _ => false,
            };
            if was_reset {
                done.clear();
                if self.reset_enabled(&u.key) && prev_pct.unwrap_or(0.0) >= 10.0 {
                    let name = limit_name(&u.key, &u.label);
                    self.notify(&format!("Your {name} has reset"), &format!("You're at {pct:.0}% used. Time to continue!"));
                }
            }
            // Re-arm a threshold if usage is clearly back above it (covers resets we missed).
            done.retain(|t| 100.0 - pct <= (*t + 5) as f64);
            if !(u.weekly && !self.settings.b("notify_weekly")) {
                let left = 100.0 - pct;
                let crossed: Vec<i64> = thresholds.iter().copied().filter(|t| left <= *t as f64 && !done.contains(t)).collect();
                if !crossed.is_empty() {
                    let name = limit_name(&u.key, &u.label);
                    self.notify(&format!("{left:.0}% of your {name} left"), &fmt_reset(&reset));
                    done.extend(crossed);
                }
            }
            self.notified.insert(
                u.key.clone(),
                json!({"done": done.iter().collect::<Vec<_>>(), "pct": pct, "reset": reset.map(|r| r.to_rfc3339())}),
            );
        }
        // Something new you can use (a reset, offer, promo...): tell once per item.
        let extras = self.credits.as_ref().map(|c| c.extras.clone()).unwrap_or_default();
        let seen: Vec<String> = self.notified.get("_extras").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        for x in &extras {
            if !seen.contains(&x.key) {
                let when = x.expires.map(|e| format!(" Expires {}.", local(&e).format("%a %d %b %H:%M"))).unwrap_or_default();
                self.notify(&format!("You have something to use: {}", x.name), &format!("Open Claude Usage for details.{when}"));
            }
        }
        self.notified.insert("_extras".into(), json!(extras.iter().map(|x| x.key.clone()).collect::<Vec<_>>())); // forget ones that are gone
        if !store::demo() {
            store::save_json(&store::path("notified.json"), &Value::Object(self.notified.clone()));
        }
    }

    fn quit(&mut self) {
        log("Quit chosen by user");
        unsafe { PostQuitMessage(0) };
    }

    fn cleanup(mut self) {
        self.mouse_hook(false);
        let nid = self.nid();
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
            if !self.tray.hicon.is_null() {
                DestroyIcon(self.tray.hicon);
            }
            DestroyWindow(self.popup);
            DestroyWindow(self.widget);
            DestroyWindow(self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_format() {
        assert_eq!(fmt_money(&Money { amount: 1234.5, currency: "USD".into() }), "$1,234.50");
        assert_eq!(fmt_money(&Money { amount: 37.6, currency: "eur".into() }), "€37.60");
        assert_eq!(fmt_money(&Money { amount: 5.0, currency: "SEK".into() }), "5.00 SEK");
    }

    #[test]
    fn names() {
        assert_eq!(short_name("weekly_fable", "Weekly, Fable"), "Fable");
        assert_eq!(limit_name("seven_day", "Weekly, all models"), "weekly limit");
        assert_eq!(limit_name("weekly_fable", "Weekly, Fable"), "weekly Fable limit");
        assert_eq!(color_for(81.0), AMBER);
        assert_eq!(color_for(96.0), RED);
    }
}
