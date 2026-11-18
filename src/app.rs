//! The app: taskbar widget, popup (usage and settings pages), tray icon, notifications.
//!
//! Pages are drawn by `view` into a layered window, so a page change swaps size and pixels
//! in one step.

use crate::draw::*;
use crate::logo::draw_logo;
use crate::store::{self, log, now, truthy, Settings, MIN_FETCH_GAP, VERSION};
use crate::usage::{self, Credits, FetchError, Limit, Money, Reading, RESET_FIELDS};
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

mod view;
use view::{pill, Seg};

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_TRAY: u32 = WM_APP + 1;
const WM_WORKER: u32 = WM_APP + 2;
const T_TICK: usize = 1;
const T_TOP: usize = 2;
const T_CLICK: usize = 3;
const T_SHOW: usize = 4;
const T_UNLOCK: usize = 5;
const T_KEYCHECK: usize = 6;

const ICON_REFRESH: char = '\u{E72C}';
const ICON_SETTINGS: char = '\u{E713}';
const ICON_BACK: char = '\u{E72B}';

/// Label border + padding around text, in pixels (not scaled).
const INSET: f32 = 2.0;
/// Padding of an image label with bd=0.
const IMG: f32 = 1.0;

enum Msg {
    Result(Result<Box<Reading>, FetchError>),
    NewVersion(String),
    /// Result of checking a new claude.ai key (its resets and credits, if it works).
    AiChecked(Option<usage::AiData>),
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Refresh,
    Settings,
    Main,
    Url(String),
    Toggle(&'static str),
    /// Dropdown: pick one value for the setting from a menu.
    Pick(&'static str, Vec<(Value, String)>),
    ResetKey(String, bool),
    Tab(&'static str),
    Login,
    Logout,
    Quit,
    Entry,
    Paste,
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

/// One word for the widget about why there are no fresh numbers.
fn error_word(e: &str) -> &'static str {
    let e = e.to_lowercase();
    if e.contains("network error") {
        "Offline"
    } else if e.contains("too many") {
        "Wait"
    } else if ["expired", "sign in", "log in", "no claude code login", "session key", "refused"].iter().any(|w| e.contains(w)) {
        "Log in"
    } else {
        "Error"
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

// ----------------------------------------------------------------- tray icon

struct Tray {
    hicon: HICON,
    sig: Option<(Vec<u8>, String)>,
    /// The logo shown in notifications (made once).
    logo: HICON,
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
    /// A new claude.ai key is being checked.
    ai_checking: bool,
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
    /// Dropdown menu for a setting, opened below (x, y) in screen pixels.
    Pick { key: &'static str, opts: Vec<(Value, String)>, x: i32, y: i32 },
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
    if let After::Pick { key, opts, x, y } = after {
        let Some((root, cur)) = APP.with(|a| {
            a.try_borrow().ok().and_then(|g| g.as_ref().map(|a| (a.root as isize, a.settings.get(key).clone())))
        }) else {
            return;
        };
        let items: Vec<Option<&str>> = opts.iter().map(|(_, t)| Some(t.as_str())).collect();
        let checked = opts.iter().position(|(v, _)| *v == cur).map(|i| i as u32 + 1);
        let cmd = win::popup_menu(root as HWND, &items, x, y, win::MenuOpts { checked, below: true, ..Default::default() });
        APP.with(|a| {
            if let Ok(mut g) = a.try_borrow_mut() {
                if let Some(app) = g.as_mut() {
                    // The menu's owner took the focus: hand it back so the popup stays open.
                    unsafe { SetForegroundWindow(app.popup) };
                    app.popup_had_focus = false;
                    if let Some((v, _)) = opts.get((cmd as usize).wrapping_sub(1)) {
                        app.set_setting(key, v.clone());
                    }
                }
            }
        });
        return;
    }
    if let After::Menu { x, y, tray } = after {
        let Some(root) = APP.with(|a| a.try_borrow().ok().and_then(|g| g.as_ref().map(|a| a.root as isize))) else { return };
        let items: Vec<Option<&str>> = if tray {
            vec![Some("Open"), Some("Refresh now"), Some("Settings"), Some("Quit")]
        } else {
            vec![Some("Open"), Some("Refresh now"), Some("Settings"), None, Some("Quit")]
        };
        let opts = win::MenuOpts { default: if tray { Some(1) } else { None }, ..Default::default() };
        let cmd = win::popup_menu(root as HWND, &items, x, y, opts);
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
            // the last hour, so the demo shows a pace line under the 5-hour bar
            // (same formula as the demo reading: +2% a minute, wrapping at 100)
            for i in (1..=20).rev() {
                let st = (t - i as f64 * 120.0).trunc();
                history.push(Sample { t: st, fh: ((st / 30.0) % 100.0).round(), sd: None });
            }
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
            tray: Tray { hicon: null_mut(), sig: None, logo: null_mut() },
            hook: null_mut(),
            login: None,
            entry_focus: false,
            entry_text: String::new(),
            entry_open: false,
            ai_checking: false,
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
        // After a restart, fetch right away if the last request is at least 5 minutes old
        // (the most often the server is asked), unless it asked us to wait longer.
        self.next_fetch = (self.last_fetch + MIN_FETCH_GAP).max(self.rate_until);
        if now() >= self.next_fetch {
            self.request_fetch();
        }
        if self.settings.b("check_updates") {
            self.check_for_update();
        }
        unsafe {
            // Hear about sign-in and unlock (WM_WTSSESSION_CHANGE) to refresh old numbers.
            win::watch_sessions(self.root);
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
                        T_KEYCHECK => {
                            unsafe { KillTimer(self.root, T_KEYCHECK) };
                            self.check_key_now();
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
                win::WM_WTSSESSION_CHANGE if wp == win::WTS_SESSION_UNLOCK || wp == win::WTS_SESSION_LOGON => {
                    self.refresh_if_old();
                    return done;
                }
                WM_POWERBROADCAST if wp == PBT_APMRESUMEAUTOMATIC as usize || wp == PBT_APMRESUMESUSPEND as usize => {
                    self.refresh_if_old();
                    return (Some(1), After::Nothing);
                }
                WM_WORKER => {
                    while let Ok(m) = self.rx.try_recv() {
                        match m {
                            Msg::Result(r) => self.on_result(r.map(|b| *b)),
                            Msg::AiChecked(d) => {
                                self.ai_checking = false;
                                // Show the result instead of the field.
                                self.entry_open = false;
                                self.entry_focus = false;
                                self.entry_text.clear();
                                if let Some(d) = d {
                                    self.credits = Some(usage::merge_ai(self.credits.take(), d));
                                }
                                if self.popup_visible() {
                                    self.render_popup();
                                }
                            }
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
                WM_MBUTTONUP => {
                    // Middle-click: refresh without opening the popup.
                    self.manual_refresh();
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
                    return (Some(0), self.popup_click(x as f32, y as f32));
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

    fn pill_image(&self, segs: &[Seg]) -> Pixmap {
        let st = &self.settings;
        pill(&self.fonts, self.scale, st.b("widget_square"), st.b("widget_outline"), self.taskbar_bg(), segs)
    }

    /// "At this pace" line for the 5-hour and weekly limits, from the recent history samples:
    /// when 100% will be reached (if before the reset), or roughly where it ends at the reset.
    fn pace(&self, u: &Limit) -> Option<(String, Rgb)> {
        // (length of the period, how far back to measure, shortest span worth trusting)
        let (period, back, min_span) = match u.key.as_str() {
            "five_hour" => (5.0 * 3600.0, 90.0 * 60.0, 10.0 * 60.0),
            "seven_day" => (7.0 * 86400.0, 86400.0, 2.0 * 3600.0),
            _ => return None,
        };
        let reset = u.resets_at?.timestamp() as f64;
        let t1 = self.updated?;
        if self.stale() || reset <= now() {
            return None;
        }
        let start = (reset - period).max(t1 - back);
        let value = |s: &Sample| if u.key == "five_hour" { Some(s.fh) } else { s.sd };
        // Measure from the oldest sample after the last drop (a reset or a new period).
        let mut first: Option<(f64, f64)> = None;
        let mut prev = f64::MIN;
        for s in self.history.iter().filter(|s| s.t >= start && s.t < t1) {
            let Some(v) = value(s) else { continue };
            if first.is_none() || v < prev {
                first = Some((s.t, v));
            }
            prev = v;
        }
        let (t0, v0) = first?;
        if t1 - t0 < min_span || u.pct < v0 {
            return None;
        }
        let rate = (u.pct - v0) / (t1 - t0); // % per second
        if rate <= 0.0 {
            return None;
        }
        let full = t1 + (100.0 - u.pct) / rate;
        if full < reset {
            let at = Local.timestamp_opt(full as i64, 0).single()?;
            let fmt = if at.date_naive() == Local::now().date_naive() { "%H:%M" } else { "%a %H:%M" };
            Some((format!("At this pace: 100% around {}", at.format(fmt)), AMBER))
        } else {
            let end = u.pct + rate * (reset - t1);
            Some((format!("At this pace: about {end:.0}% at the reset"), MUTED))
        }
    }

    /// The numbers are too old to trust: no successful update for 3 refresh periods (at least 15 min).
    fn stale(&self) -> bool {
        let max_age = (3 * self.settings.i("refresh_minutes")).max(15) as f64 * 60.0;
        self.updated.is_some_and(|t| now() - t > max_age)
    }

    fn update_widget(&mut self) {
        let st = &self.settings;
        let fresh = if self.stale() { None } else { self.usage.as_ref() };
        let segs: Vec<Seg> = match fresh {
            Some(usage) => {
                let show = st.s("widget_show");
                let mut keys: Vec<&str> = match show.as_str() {
                    "five_hour" | "seven_day" | "both" => {
                        [("five_hour", show != "seven_day"), ("seven_day", show != "five_hour")].into_iter().filter(|(_, on)| *on).map(|(k, _)| k).collect()
                    }
                    other => vec![other], // a per-model limit, like weekly_fable
                };
                keys.retain(|k| usage.iter().any(|l| l.key == *k));
                if keys.is_empty() {
                    keys.push("five_hour");
                }
                let both = keys.len() > 1;
                keys.iter()
                    .filter_map(|k| usage.iter().find(|l| l.key == *k))
                    .map(|u| {
                        let timer = if st.b("widget_timer") { short_left(&u.resets_at) } else { String::new() };
                        let label = if both { if u.key == "five_hour" { "5h" } else { "W" } } else { "" };
                        if u.resets_at.is_some_and(|r| r <= Utc::now()) {
                            // reset already happened: the old number is wrong until the next update
                            return (label.to_string(), "--%".into(), String::new(), MUTED);
                        }
                        (label.to_string(), format!("{:.0}%", u.pct), timer, color_for(u.pct))
                    })
                    .collect()
            }
            // No usable numbers: say why in a word (Offline, Log in...) instead of a bare "!".
            None => vec![(String::new(), self.error.as_deref().map_or("--%", error_word).into(), String::new(), MUTED)],
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
        let fresh = if self.stale() { None } else { self.usage.as_ref() };
        let shown = fresh.and_then(|u| u.iter().find(|l| l.key == key)).filter(|l| !l.resets_at.is_some_and(|r| r <= Utc::now()));
        match shown {
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
        let title = match &self.usage {
            Some(usage) => {
                // "5h 39% · resets 00:39", "Week 19% · resets Mon 14:59"
                let lines: Vec<String> = usage
                    .iter()
                    .filter(|l| l.key == "five_hour" || l.key == "seven_day")
                    .map(|l| {
                        let name = if l.key == "five_hour" { "5h" } else { "Week" };
                        match l.resets_at {
                            Some(r) if r <= Utc::now() => format!("{name} reset at {}", local(&r).format("%H:%M")),
                            Some(r) => {
                                let fmt = if local(&r).date_naive() == Local::now().date_naive() { "%H:%M" } else { "%a %H:%M" };
                                format!("{name} {:.0}% · resets {}", l.pct, local(&r).format(fmt))
                            }
                            None => format!("{name} {:.0}%", l.pct),
                        }
                    })
                    .collect();
                let head = if self.stale() { "Claude usage · out of date" } else { "Claude usage" };
                let mut t = format!("{head}\n{}", lines.join("\n"));
                if let Some(e) = &self.error {
                    // "Offline · numbers from 14:05"
                    let when = self.updated.and_then(|u| Local.timestamp_opt(u as i64, 0).single()).map(|d| d.format("%H:%M").to_string());
                    t += &format!("\n{}{}", error_word(e), when.map(|w| format!(" · numbers from {w}")).unwrap_or_default());
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

    fn notify(&mut self, title: &str, msg: &str) {
        if self.tray.logo.is_null() {
            // Large notification icon: the app logo at the system's large icon size.
            let n = unsafe { GetSystemMetrics(SM_CXICON) }.max(32) as u32;
            self.tray.logo = win::make_icon(&draw_logo(n, 0.03, None));
        }
        let mut nid = self.nid();
        nid.uFlags = NIF_INFO;
        copy_w(&mut nid.szInfo, msg);
        copy_w(&mut nid.szInfoTitle, title);
        nid.dwInfoFlags = if self.tray.logo.is_null() { NIIF_NONE } else { NIIF_USER | NIIF_LARGE_ICON };
        nid.hBalloonIcon = self.tray.logo;
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

    fn popup_click(&mut self, x: f32, y: f32) -> After {
        let hit = self.hit_at(x, y).and_then(|i| self.page.as_ref().map(|p| (p.hits[i].action.clone(), p.hits[i].r)));
        let action = hit.as_ref().map(|(a, _)| a.clone());
        let was_focus = self.entry_focus;
        self.entry_focus = action == Some(Action::Entry);
        let Some(action) = action else {
            if was_focus {
                self.render_popup();
            }
            return After::Nothing;
        };
        if let (Action::Pick(key, opts), Some((_, r))) = (&action, &hit) {
            // Open the menu right under the dropdown button.
            let rc = win::rect_of(self.popup);
            return After::Pick { key, opts: opts.clone(), x: rc.left + r.0 as i32, y: rc.top + r.3 as i32 };
        }
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
            Action::Paste => {
                if let Some(t) = win::clipboard_text(self.popup) {
                    // The whole field is replaced: a pasted key is the complete value.
                    let key = t.chars().filter(|c| !c.is_control()).collect::<String>();
                    if !key.trim().is_empty() {
                        // A pasted key is complete: check it now and show the result.
                        self.entry_text.clear();
                        self.entry_open = false;
                        self.entry_focus = false;
                        self.store_session_key(&key);
                        self.check_key_now();
                    }
                }
                self.render_popup();
            }
            Action::Entry => {
                self.entry_open = true;
                self.render_popup();
            }
            Action::Pick(..) => {} // handled above: the menu opens after this returns
        }
        After::Nothing
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
                self.notify_login_failed();
                return;
            }
        }
        self.hide_popup();
    }

    fn check_login(&mut self) {
        let Some(child) = self.login.as_mut() else { return };
        let opened = match child.try_wait() {
            Ok(None) => return, // still open
            Ok(Some(status)) => status.success(),
            Err(_) => false,
        };
        self.login = None;
        let result = store::path("login_result.json");
        // The login window hands the key over encrypted.
        let key = store::load_json(&result)
            .and_then(|v| v.get("key_enc").and_then(|k| k.as_str()).and_then(crate::secret::unprotect))
            .unwrap_or_default();
        let _ = std::fs::remove_file(&result);
        if !key.is_empty() {
            self.store_session_key(&key);
            self.check_key_now();
            self.notify("Logged in to claude.ai", "Limit resets and credits will show in Claude Usage.");
            log("Logged in to claude.ai");
        } else if opened {
            self.notify("Not logged in to claude.ai", "The login window was closed before you logged in. Try again on the Account tab.");
        } else {
            self.notify_login_failed();
        }
        if self.popup_visible() && self.view == View::Settings {
            self.render_popup();
        }
    }

    fn notify_login_failed(&mut self) {
        self.notify(
            "Couldn't open the login window",
            "Microsoft Edge WebView2 may be missing. You can paste the sessionKey cookie on the Account tab instead.",
        );
    }

    fn logout_claude_ai(&mut self) {
        self.settings.set_session_key("");
        self.settings.save();
        usage::set_ai_status(usage::AiStatus::Unknown);
        usage::set_ai_cache(0.0, None);
        store::save_fetch_state(&[("ai_t", json!(0)), ("ai_data", Value::Null)]);
        if let Some(c) = self.credits.as_mut() {
            c.extras.retain(|x| !RESET_FIELDS.contains(&x.key.split(':').next().unwrap_or("")));
        }
        self.render_popup();
    }

    /// Save a new key (encrypted) and forget what the old one said.
    fn store_session_key(&mut self, value: &str) {
        self.settings.set_session_key(value);
        self.settings.save();
        usage::set_ai_status(usage::AiStatus::Unknown);
        self.next_fetch = now() + 2.0; // the usage reading itself keeps its 5-minute limit
    }

    /// A key typed or pasted in the field: check it once typing stops.
    fn set_session_key(&mut self, value: &str) {
        self.store_session_key(value);
        unsafe { SetTimer(self.root, T_KEYCHECK, 1500, None) };
    }

    /// Ask claude.ai whether the key works and get its resets and credits right away.
    fn check_key_now(&mut self) {
        if !self.settings.has_session_key() || self.ai_checking {
            return;
        }
        self.ai_checking = true;
        let settings = self.settings.clone();
        let post = self.post();
        std::thread::spawn(move || post(Msg::AiChecked(usage::check_ai_now(&settings))));
        if self.popup_visible() {
            self.render_popup();
        }
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

    /// Back at the PC (sign-in, unlock, wake): fetch soon if the numbers are over 5 minutes old.
    /// A few seconds' wait gives the network time to come back after sleep.
    fn refresh_if_old(&mut self) {
        let old = self.updated.is_none_or(|t| now() - t > 5.0 * 60.0);
        if old && self.next_fetch > now() + 5.0 {
            self.next_fetch = now() + 5.0;
        }
        self.update_widget(); // grey out numbers that went stale while away
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
        std::thread::spawn(move || post(Msg::Result(usage::fetch(&settings).map(Box::new))));
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
                // Only when this much (or less) was left before the reset; 100 = always.
                let max_left = self.settings.i("notify_reset_left") as f64;
                if self.reset_enabled(&u.key) && 100.0 - prev_pct.unwrap_or(0.0) <= max_left {
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
            if !self.tray.logo.is_null() {
                DestroyIcon(self.tray.logo);
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
        assert_eq!(error_word("network error: Dns / network error: Dns"), "Offline");
        assert_eq!(error_word("claude.ai session expired or not valid. Log in again on the Account tab."), "Log in");
        assert_eq!(error_word("Claude Code login expired. Open Claude Code to renew it."), "Log in");
        assert_eq!(error_word("Too many usage requests right now. Trying again at 14:05."), "Wait");
        assert_eq!(error_word("Usage request failed (HTTP 500)"), "Error");
        assert_eq!(limit_name("seven_day", "Weekly, all models"), "weekly limit");
        assert_eq!(limit_name("weekly_fable", "Weekly, Fable"), "weekly Fable limit");
        assert_eq!(color_for(81.0), AMBER);
        assert_eq!(color_for(96.0), RED);
    }
}
