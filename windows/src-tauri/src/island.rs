// Island window: placement on the chosen display and the two window sizes
// (full panel / invisible wake strip).
//
// There is no notch on a PC, so the island is a black shape drawn at the top
// centre of the main display inside a borderless, transparent, always-on-top
// window that should not take focus.
//
// Linux/Wayland differences from the Windows build, stated honestly:
// * Wayland does not let an ordinary client read the global pointer position,
//   so there is no cursor poll thread. Eye tracking and hover wake are driven
//   by the front end from DOM mouse events inside the window (see main.ts).
// * The window rect *is* the hit region: click-through stays off, so the panel
//   — and the 240x6 wake strip while collapsed — takes the mouse everywhere.
//   Clicks on the transparent margins of the rounded island shape are swallowed;
//   that is the price of not having GetCursorPos, and the margin is small.
// * Screen placement uses the Tauri monitor APIs. `set_position` on Wayland is
//   honoured by KWin only inasmuch as the compositor allows client-side
//   placement; if the island lands off-centre on a Wayland session, that is the
//   first place to look (see apply_geometry).
// * Focus: tauri.conf.json starts the window with `focus: false`, show() never
//   activates it on X11/KWin by policy, and we only call set_focus() when a
//   text field inside the island must type (focus_window command).

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

/// Logical size of the full window — the largest island view, like the macOS panel.
pub const PANEL_W: f64 = 720.0;
pub const PANEL_H: f64 = 320.0;
/// Logical height of the wake strip that reveals the island when it is hidden.
pub const STRIP_H: f64 = 6.0;

pub const WINDOW_LABEL: &str = "island";

#[derive(Serialize, Clone)]
pub struct ScreenInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

/// Collapsed (wake strip) is the only window-state bit Rust owns now; the
/// front end decides visibility, Rust decides geometry.
pub struct PollGate {
    pub collapsed: AtomicBool,
}

impl PollGate {
    pub fn new() -> Self {
        Self { collapsed: AtomicBool::new(true) }
    }

    pub fn set_collapsed(&self, collapsed: bool) {
        self.collapsed.store(collapsed, Ordering::Relaxed);
    }
}

pub fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW_LABEL)
}

/// The display the island lives on: the primary one.
///
/// The "cursor" preference (display under the pointer) is accepted but resolves
/// to the primary display: Wayland does not expose the global pointer position
/// to ordinary clients, and this function has no honest way to answer it. The
/// front end still offers the option; this is where it degrades.
fn target_monitor(app: &AppHandle, pref: &str) -> Option<Monitor> {
    let _ = pref;
    let monitors = app.available_monitors().ok()?;
    app.primary_monitor()
        .ok()
        .flatten()
        .or_else(|| monitors.into_iter().next())
}

pub fn screen_info(app: &AppHandle, pref: &str) -> ScreenInfo {
    match target_monitor(app, pref) {
        Some(m) => {
            let scale = m.scale_factor();
            let p = m.position();
            let s = m.size();
            ScreenInfo {
                x: p.x as f64 / scale,
                y: p.y as f64 / scale,
                width: s.width as f64 / scale,
                height: s.height as f64 / scale,
                scale,
            }
        }
        None => ScreenInfo { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0 },
    }
}

/// Places and sizes the window. `collapsed` picks the wake strip instead of the panel.
pub fn apply_geometry(app: &AppHandle, pref: &str, collapsed: bool) {
    let Some(win) = window(app) else {
        crate::log::line("geometry: island window missing".to_string());
        return;
    };
    let Some(m) = target_monitor(app, pref) else {
        crate::log::line("geometry: no monitor available".to_string());
        return;
    };

    let scale = m.scale_factor();
    let mp = *m.position();
    let ms = *m.size();

    // The window is always PANEL_W wide. On Wayland the compositor decides x
    // (a client-side set_position is ignored) and the KWin rule pins us to
    // top-center for exactly this width — keeping one width makes that static
    // rule exact for both states. Collapsing only drops the height.
    let (lw, lh) = if collapsed { (PANEL_W, STRIP_H) } else { (PANEL_W, PANEL_H) };
    let pw = (lw * scale).round().max(1.0) as u32;
    let ph = (lh * scale).round().max(1.0) as u32;
    let x = mp.x + (ms.width as i32 - pw as i32) / 2;
    let y = mp.y;

    let size_result = win.set_size(PhysicalSize::new(pw, ph));
    let pos_result = win.set_position(PhysicalPosition::new(x, y));
    // Moving across displays can rescale the window: re-assert the physical size.
    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_always_on_top(true);
    // The window rect is the hit region (no cursor to make it finer); make sure
    // the flag is on whatever happened before a resize.
    let _ = win.set_ignore_cursor_events(false);
    crate::log::line(format!(
        "geometry {} {}x{} at ({x},{y}) scale={scale} size={:?} pos={:?}",
        if collapsed { "strip" } else { "panel" },
        pw,
        ph,
        size_result,
        pos_result,
    ));
}

/// Position, size and scale of the monitor the island lives on. Any change here
/// means the island has to be placed again.
fn current_screen_key(app: &AppHandle) -> Option<(i32, i32, u32, u32, u64)> {
    let pref = app
        .try_state::<crate::Shared>()
        .map(|s| s.settings.lock().screen.clone())
        .unwrap_or_else(|| "primary".into());
    let m = target_monitor(app, &pref)?;
    let p = m.position();
    let size = m.size();
    Some((p.x, p.y, size.width, size.height, m.scale_factor().to_bits()))
}

/// Watches the monitor layout at ~2 Hz while the app runs and tells the front
/// end to re-place the island when a display changed. This replaces the Windows
/// cursor poll thread: monitors still get plugged, unplugged, rearranged and
/// rescaled, and an island pinned to coordinates that no longer exist is an
/// island nobody can reach. No cursor is involved, so this can run cheaply all
/// the time instead of only while the island is visible.
pub fn spawn_screen_watch(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_screen: Option<(i32, i32, u32, u32, u64)> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let now = current_screen_key(&app);
            if now.is_some() && now != last_screen {
                let first = last_screen.is_none();
                last_screen = now;
                if !first {
                    crate::log::line("display layout changed — repositioning".to_string());
                    let _ = app.emit_to(WINDOW_LABEL, "screen-changed", ());
                }
            }
        }
    });
}
