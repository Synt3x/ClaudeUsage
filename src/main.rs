//! Claude Usage - shows your Claude plan usage next to the Windows clock.
//!
//! Run:  ClaudeUsage.exe           (normal)
//!       ClaudeUsage.exe --demo    (fake data, for trying out the UI)
#![windows_subsystem = "windows"]

mod app;
mod draw;
#[cfg(feature = "login")]
mod login;
mod logo;
mod store;
mod usage;
mod win;

fn main() {
    std::panic::set_hook(Box::new(|info| store::log(&format!("Crash: {info}"))));
    if store::has_arg("--version") {
        std::process::exit(0);
    }
    if store::has_arg("--login") {
        #[cfg(feature = "login")]
        login::run();
        return;
    }
    // Only one instance at a time (same mutex name as the Python version).
    // A --demo copy can run next to the real one.
    let name = win::wide(if store::demo() { "Local\\ClaudeUsageTrayAppDemo" } else { "Local\\ClaudeUsageTrayApp" });
    let _mutex = unsafe { windows_sys::Win32::System::Threading::CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if unsafe { windows_sys::Win32::Foundation::GetLastError() } == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS {
        // Say so instead of quietly doing nothing; only one copy can run at a time.
        win::message_box(
            "Claude Usage is already running.\n\nLook for it next to the clock or in the tray. \
             To start this copy instead, quit the running one first (right-click it → Quit).",
            "Claude Usage",
        );
        return;
    }
    store::log("Started");
    app::run();
    store::log("Stopped");
}
