//! Files in %APPDATA%\ClaudeUsage, settings and the log.
//!
//! The file names and JSON layout match the Python version, so switching keeps settings,
//! history and the last reading.

use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub const APP_NAME: &str = "ClaudeUsage";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Never ask the server more often than this, also across restarts.
pub const MIN_FETCH_GAP: f64 = 5.0 * 60.0;

pub fn args() -> &'static [String] {
    static A: OnceLock<Vec<String>> = OnceLock::new();
    A.get_or_init(|| std::env::args().collect())
}

pub fn has_arg(a: &str) -> bool {
    args().iter().any(|x| x == a)
}

/// `--demo`: fake data for trying out the UI; nothing is read from or sent to any account.
pub fn demo() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| has_arg("--demo"))
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
    });
    base.join(APP_NAME)
}

pub fn path(name: &str) -> PathBuf {
    data_dir().join(name)
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn load_json(p: &std::path::Path) -> Option<Value> {
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write via a temporary file so a crash never leaves half a file behind.
pub fn save_json(p: &std::path::Path, data: &Value) {
    let res = (|| -> std::io::Result<()> {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut buf = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
        serde::Serialize::serialize(data, &mut ser).map_err(std::io::Error::other)?;
        let tmp = p.with_extension("json.tmp");
        fs::write(&tmp, &buf)?;
        fs::rename(&tmp, p)
    })();
    if let Err(e) = res {
        log(&format!("Couldn't save {}: {e}", p.file_name().unwrap_or_default().to_string_lossy()));
    }
}

/// Python-style truthiness: null, false, 0, "", [] and {} are false.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ----------------------------------------------------------------- log

thread_local! {
    static LOGGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Append a line to %APPDATA%\ClaudeUsage\app.log (kept small).
/// Never pass tokens, cookies or headers in here.
pub fn log(msg: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    // try_lock: a panic while logging calls log() again from the panic hook; waiting here
    // would hang the app forever.
    let _g = match LOCK.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            if LOGGING.with(|l| l.get()) {
                return;
            }
            LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }
    };
    LOGGING.with(|l| l.set(true));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            LOGGING.with(|l| l.set(false));
        }
    }
    let _reset = Reset;
    let _ = (|| -> std::io::Result<()> {
        let dir = data_dir();
        fs::create_dir_all(&dir)?;
        let p = dir.join("app.log");
        if fs::metadata(&p).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
            let _ = fs::rename(&p, dir.join("app.log.old"));
        }
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&p)?;
        let t = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        writeln!(f, "{t} v{VERSION} {msg}")
    })();
}

// ----------------------------------------------------------------- fetch state

/// Saved time of the last request (and any rate-limit wait), also used across restarts.
pub fn save_fetch_state(values: &[(&str, Value)]) {
    if demo() {
        return;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock();
    let p = path("fetch_state.json");
    let mut st = match load_json(&p) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for (k, v) in values {
        st.insert((*k).to_string(), v.clone());
    }
    save_json(&p, &Value::Object(st));
}

pub fn load_fetch_state() -> Map<String, Value> {
    if demo() {
        return Map::new();
    }
    match load_json(&path("fetch_state.json")) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

// ----------------------------------------------------------------- settings

fn defaults() -> Map<String, Value> {
    let v = json!({
        "autostart": false,
        "check_updates": true,        // look for a newer release on GitHub at startup
        "show_taskbar_widget": true,
        "show_credits": true,         // popup: credits, resets and the usage page link
        "show_chart": true,           // popup: this week's graph
        "notify_20": true,
        "notify_10": true,
        "notify_5": true,
        "notify_reset": true,         // default for limit types not in notify_reset_keys
        "notify_reset_keys": {},      // per limit type: {"five_hour": true, "weekly_fable": false, ...}
        "notify_weekly": true,
        "refresh_minutes": 5,
        "source": "auto",             // auto | claude_code | claude_ai
        "allow_token_refresh": true,  // refresh Claude Code's login when it has expired
        "session_key": "",            // claude.ai sessionKey cookie
        "widget_square": true,        // small colored square in the taskbar widget
        "widget_outline": true,       // rounded outline around the taskbar widget
        "widget_timer": true,         // reset countdown in the taskbar widget
        "widget_five": true,          // 5-hour usage in the taskbar widget
        "widget_weekly": false,       // weekly usage in the taskbar widget
        "tray_show": "five_hour",     // tray icon number: off | five_hour | seven_day
        "widget_offset": 0,           // px the user dragged the taskbar widget
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

#[derive(Clone)]
pub struct Settings(pub Map<String, Value>);

impl Settings {
    pub fn load() -> Settings {
        let mut s = defaults();
        if let Some(Value::Object(saved)) = load_json(&path("settings.json")) {
            for (k, v) in saved {
                s.insert(k, v);
            }
        }
        let mut st = Settings(s);
        let mins = st.i("refresh_minutes").max((MIN_FETCH_GAP / 60.0) as i64);
        st.0.insert("refresh_minutes".into(), json!(mins));
        st
    }

    pub fn save(&self) {
        save_json(&path("settings.json"), &Value::Object(self.0.clone()));
    }

    pub fn get(&self, k: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.0.get(k).unwrap_or(&NULL)
    }

    pub fn b(&self, k: &str) -> bool {
        truthy(self.get(k))
    }

    pub fn i(&self, k: &str) -> i64 {
        let v = self.get(k);
        v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0)
    }

    pub fn s(&self, k: &str) -> String {
        self.get(k).as_str().unwrap_or("").to_string()
    }

    pub fn set(&mut self, k: &str, v: Value) {
        self.0.insert(k.to_string(), v);
    }

    pub fn session_key(&self) -> String {
        self.s("session_key").trim().to_string()
    }
}

// ----------------------------------------------------------------- autostart

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Point the Windows startup entry at the exe that is running now.
pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let k = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)?;
    if enabled {
        let exe = std::env::current_exe()?;
        k.set_value(APP_NAME, &format!("\"{}\"", exe.display()))
    } else {
        match k.delete_value(APP_NAME) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}
