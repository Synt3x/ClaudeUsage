//! Login window (`ClaudeUsage.exe --login`): claude.ai in an embedded WebView2 browser.
//! Once signed in, the sessionKey cookie is written to login_result.json for the main app
//! and the window closes. The cookie value is never logged.

use crate::logo::{draw_logo, rgba};
use crate::store::{self, log};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::{Icon, WindowBuilder};
use wry::{WebContext, WebViewBuilder};

pub const LOGIN_TITLE: &str = "Log in to claude.ai";

/// Runs the login window; returns false if it couldn't be opened.
pub fn run() -> bool {
    match run_inner() {
        Ok(()) => true,
        Err(e) => {
            log(&format!("Login window failed: {e}"));
            false
        }
    }
}

fn run_inner() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new();
    let icon = draw_logo(64, 0.03, None);
    let window = WindowBuilder::new()
        .with_title(LOGIN_TITLE)
        .with_inner_size(LogicalSize::new(480.0, 760.0))
        .with_window_icon(Icon::from_rgba(rgba(&icon), 64, 64).ok())
        .build(&event_loop)?;
    // Private mode: nothing is kept on disk except the key we hand back.
    let mut ctx = WebContext::new(Some(store::data_dir().join("WebView2")));
    let webview = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_url("https://claude.ai/login")
        .with_incognito(true)
        .build(&window)?;
    let started = Instant::now();
    let mut next = Instant::now() + Duration::from_secs(1);
    event_loop.run(move |event, _, flow| {
        let _keep = (&window, &ctx);
        *flow = ControlFlow::WaitUntil(next);
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *flow = ControlFlow::Exit,
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                next = Instant::now() + Duration::from_secs(1);
                *flow = ControlFlow::WaitUntil(next);
                if started.elapsed() > Duration::from_secs(20 * 60) {
                    *flow = ControlFlow::Exit; // give up after 20 minutes
                    return;
                }
                let Ok(cookies) = webview.cookies_for_url("https://claude.ai") else { return };
                if let Some(c) = cookies.iter().find(|c| c.name() == "sessionKey" && c.value().starts_with("sk-ant-")) {
                    store::save_json(&store::path("login_result.json"), &serde_json::json!({"key": c.value()}));
                    *flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}
