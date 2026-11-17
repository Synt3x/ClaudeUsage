//! Win32 helpers: taskbar position, fullscreen detection, layered windows, icons, menus.

use crate::draw::{bgra_premul, bgra_straight};
use std::ptr::null_mut;
use tiny_skia::Pixmap;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn rect_of(h: HWND) -> RECT {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetWindowRect(h, &mut rc) };
    rc
}

pub fn point_in(h: HWND, x: i32, y: i32) -> bool {
    let rc = rect_of(h);
    rc.left <= x && x < rc.right && rc.top <= y && y < rc.bottom
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 64) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// A native menu (ours or the tray's) is open.
pub fn menu_open() -> bool {
    class_name(unsafe { GetForegroundWindow() }) == "#32768"
}

pub fn tray_wnd() -> HWND {
    let cls = wide("Shell_TrayWnd");
    unsafe { FindWindowW(cls.as_ptr(), std::ptr::null()) }
}

pub fn system_scale() -> f32 {
    let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// (right_edge_x, top_y, height) of the area just left of the tray, or None.
pub fn taskbar_anchor(scale: f32) -> Option<(i32, i32, i32)> {
    let tray = tray_wnd();
    if tray.is_null() {
        return None;
    }
    let rc = rect_of(tray);
    if rc.bottom - rc.top > rc.right - rc.left {
        return None; // vertical taskbar: not supported, use the tray icon
    }
    let cls = wide("TrayNotifyWnd");
    let notify = unsafe { FindWindowExW(tray, null_mut(), cls.as_ptr(), std::ptr::null()) };
    let mut right = rc.right - (300.0 * scale) as i32;
    if !notify.is_null() {
        let nr = rect_of(notify);
        if nr.right > nr.left {
            right = nr.left;
        }
    }
    Some((right, rc.top, rc.bottom - rc.top))
}

/// True when the taskbar can't be seen, so the widget shouldn't show either: a fullscreen
/// window (remote desktop, a game, a video) is active on the taskbar's monitor, or the
/// taskbar is auto-hidden and slid off the screen.
pub fn taskbar_covered(own: &[HWND]) -> bool {
    unsafe {
        let tray = tray_wnd();
        if tray.is_null() {
            return false;
        }
        let mon = MonitorFromWindow(tray, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if mon.is_null() || GetMonitorInfoW(mon, &mut mi) == 0 {
            return false;
        }
        let m = mi.rcMonitor;
        let tr = rect_of(tray);
        let vis_w = tr.right.min(m.right) - tr.left.max(m.left);
        let vis_h = tr.bottom.min(m.bottom) - tr.top.max(m.top);
        let area = (tr.right - tr.left) as i64 * (tr.bottom - tr.top) as i64;
        if area > 0 && (vis_w.max(0) as i64) * (vis_h.max(0) as i64) < area / 2 {
            return true; // auto-hidden
        }
        let fg = GetForegroundWindow();
        if fg.is_null() || own.contains(&fg) {
            return false;
        }
        // The desktop also covers the whole monitor, so it must not count as a fullscreen app.
        let c = class_name(fg);
        if ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"].contains(&c.as_str())
            || MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) != mon
        {
            return false;
        }
        let r = rect_of(fg);
        // A maximized window stops at the visible taskbar, so only a fullscreen one covers it all.
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

pub fn work_area() -> RECT {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rc as *mut _ as *mut _, 0) };
    rc
}

/// Screen color at (x, y), or None.
pub fn screen_pixel(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    unsafe {
        let dc = GetDC(null_mut());
        let c = GetPixel(dc, x, y);
        ReleaseDC(null_mut(), dc);
        if c == CLR_INVALID {
            return None;
        }
        Some(((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8))
    }
}

/// Show `pm` as the content of a layered window at (x, y), replacing size, position and
/// pixels in one step (no flicker).
pub fn update_layered(h: HWND, pm: &Pixmap, x: i32, y: i32) {
    let (w, ht) = (pm.width() as i32, pm.height() as i32);
    unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -ht; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let bmp = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !bmp.is_null() && !bits.is_null() {
            let src = bgra_premul(pm);
            std::ptr::copy_nonoverlapping(src.as_ptr(), bits as *mut u8, src.len());
            let old = SelectObject(mem, bmp);
            let pt = POINT { x, y };
            let size = SIZE { cx: w, cy: ht };
            let zero = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(h, screen, &pt, &size, mem, &zero, 0, &blend, ULW_ALPHA);
            SelectObject(mem, old);
            DeleteObject(bmp);
        }
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
    }
}

/// An HICON from a pixmap (straight alpha). Destroy it with DestroyIcon.
pub fn make_icon(pm: &Pixmap) -> HICON {
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let dc = GetDC(null_mut());
        let color = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        ReleaseDC(null_mut(), dc);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        let src = bgra_straight(pm);
        std::ptr::copy_nonoverlapping(src.as_ptr(), bits as *mut u8, src.len());
        let mask_bits = vec![0u8; (((w + 15) / 16 * 2) * h) as usize];
        let mask = CreateBitmap(w, h, 1, 1, mask_bits.as_ptr() as *const _);
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

pub fn open_url(url: &str) {
    let op = wide("open");
    let u = wide(url);
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), u.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
}

pub fn clipboard_text(owner: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(owner) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(13); // CF_UNICODETEXT
        if !h.is_null() {
            let p = GlobalLock(h) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        out
    }
}

/// Dark native menus (uxtheme SetPreferredAppMode(ForceDark) + FlushMenuThemes).
pub fn dark_menus() {
    unsafe {
        let name = wide("uxtheme.dll");
        let ux = LoadLibraryW(name.as_ptr());
        if ux.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(ux, 135 as *const u8) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_mode(2);
        }
        if let Some(f) = GetProcAddress(ux, 136 as *const u8) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

/// Native popup menu at (x, y); returns the chosen item's 1-based index, or 0.
/// `items`: None is a separator. `default` makes that index bold.
pub fn popup_menu(owner: HWND, items: &[Option<&str>], x: i32, y: i32, default: Option<u32>) -> u32 {
    unsafe {
        let m = CreatePopupMenu();
        for (i, it) in items.iter().enumerate() {
            match it {
                None => {
                    AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
                }
                Some(t) => {
                    let w = wide(t);
                    AppendMenuW(m, MF_STRING, i + 1, w.as_ptr());
                }
            }
        }
        if let Some(d) = default {
            SetMenuDefaultItem(m, d, 0);
        }
        SetForegroundWindow(owner); // required so the menu closes when clicking elsewhere
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, owner, std::ptr::null());
        PostMessageW(owner, WM_NULL, 0, 0);
        DestroyMenu(m);
        cmd.max(0) as u32
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) };
    (pt.x, pt.y)
}

pub fn message_box(text: &str, title: &str) {
    let t = wide(text);
    let c = wide(title);
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_ICONINFORMATION | MB_TOPMOST) };
}
