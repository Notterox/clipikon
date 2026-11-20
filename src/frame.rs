//! Red frame around the recorded area plus a small timer/stop pill (or, before a delayed start, a
//! countdown/cancel pill). Both are excluded from capture (WDA_EXCLUDEFROMCAPTURE) and never take focus
//! away from the game. The pill is a per-pixel-alpha window (see `layered`), so it's opaque and crisp.

use crate::displays::CaptureRegion;
use crate::layered::{Canvas, argb};
use crate::win::*;
use std::cell::RefCell;
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const BORDER_CLASS: PCWSTR = w!("ClipikonBorder");
const PILL_CLASS: PCWSTR = w!("ClipikonPill");
const KEY: windows::Win32::Foundation::COLORREF = rgb(255, 0, 254);
const RED: windows::Win32::Foundation::COLORREF = rgb(0xE5, 0x48, 0x4D);
const THICKNESS: i32 = 3; // physical px

struct State {
    owner: HWND,
    border: HWND,
    pill: HWND,
    pill_pos: POINT,
    canvas: Option<Canvas>,
    scale: f64,
    started: Instant,
    /// Set while counting down to a delayed start.
    countdown_end: Option<Instant>,
    /// In pill-local pixels.
    stop_button: RECT,
}

thread_local! {
    static FRAME: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Lets screenshots see the frame and pill (normally excluded from capture); for `--ui-test`.
static CAPTURABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_capturable(on: bool) {
    CAPTURABLE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// `countdown_end`: show "Starting in N" with a Cancel button until then, instead of the recording timer.
/// The button posts `WM_APP_STOP` to `owner` either way.
pub fn show(owner: HWND, r: &CaptureRegion, countdown_end: Option<Instant>) {
    register_class(BORDER_CLASS, border_proc, IDC_ARROW);
    register_class(PILL_CLASS, pill_proc, IDC_HAND);
    let d = &r.display;
    let scale = scale_at(r.screen_x() + r.width / 2, r.screen_y() + r.height / 2);
    let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;

    // The frame sits just outside the region (clamped to the monitor), so it isn't in the shot either way.
    let br = RECT {
        left: (r.screen_x() - THICKNESS).max(d.left),
        top: (r.screen_y() - THICKNESS).max(d.top),
        right: (r.screen_x() + r.width + THICKNESS).min(d.left + d.width),
        bottom: (r.screen_y() + r.height + THICKNESS).min(d.top + d.height),
    };
    let border = create_window(ex | WS_EX_TRANSPARENT, BORDER_CLASS, WS_POPUP, br);

    let (w, h, gap) = ((248.0 * scale) as i32, (52.0 * scale) as i32, (10.0 * scale) as i32);
    let x = (r.screen_x() + (r.width - w) / 2).clamp(d.left, d.left + d.width - w);
    let mut y = r.screen_y() + r.height + gap; // below the region…
    if y + h > d.top + d.height {
        y = r.screen_y() - gap - h; // …or above it…
    }
    if y < d.top {
        y = r.screen_y() + r.height - gap - h; // …or inside the bottom edge (it's excluded from capture)
    }
    let pill = create_window(ex, PILL_CLASS, WS_POPUP, RECT { left: x, top: y, right: x + w, bottom: y + h });

    let state = State {
        owner,
        border,
        pill,
        pill_pos: POINT { x, y },
        canvas: Canvas::new(w, h),
        scale,
        started: Instant::now(),
        countdown_end,
        stop_button: RECT::default(),
    };
    FRAME.with(|f| *f.borrow_mut() = Some(state));
    render_pill();
    unsafe {
        let _ = SetLayeredWindowAttributes(border, KEY, 255, LWA_COLORKEY);
        for hwnd in [border, pill] {
            if !CAPTURABLE.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        SetTimer(Some(pill), 1, 250, None);
    }
}

pub fn close() {
    if let Some(s) = FRAME.with(|f| f.borrow_mut().take()) {
        unsafe {
            let _ = DestroyWindow(s.border);
            let _ = DestroyWindow(s.pill);
        }
    }
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd, |hdc, rc| {
                fill(hdc, rc, RED);
                let inner = RECT { left: rc.left + THICKNESS, top: rc.top + THICKNESS, right: rc.right - THICKNESS, bottom: rc.bottom - THICKNESS };
                fill(hdc, inner, KEY);
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe extern "system" fn pill_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER => {
            render_pill();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
            let owner = FRAME.with(|f| {
                let f = f.borrow();
                let s = f.as_ref()?;
                let b = s.stop_button;
                (x >= b.left && x < b.right && y >= b.top && y < b.bottom).then_some(s.owner)
            });
            if let Some(owner) = owner {
                unsafe {
                    let _ = PostMessageW(Some(owner), WM_APP_STOP, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Fluent-style pill: a dark rounded panel with a status dot, the timer (or countdown) and a Stop (or Cancel)
/// button.
fn render_pill() {
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        let Some(s) = f.as_mut() else { return };
        let Some(c) = s.canvas.as_mut() else { return };
        let px = |v: f64| (v * s.scale).round() as i32;
        let (w, h) = (c.w, c.h);
        c.clear();
        c.panel(RECT { left: 0, top: 0, right: w, bottom: h }, px(12.0) as f32, argb(255, 0x26, 0x26, 0x26), argb(255, 0x45, 0x45, 0x45));

        let (text, label, dot) = match s.countdown_end {
            Some(end) => {
                let left = end.saturating_duration_since(Instant::now()).as_secs_f64().ceil() as u64;
                (format!("Starting in {left}"), "Cancel", argb(255, 0xF0, 0xA2, 0x3B))
            }
            None => {
                let t = s.started.elapsed();
                let secs = t.as_secs();
                // The record dot blinks: on for most of each second.
                let on = t.subsec_millis() < 700;
                (format!("{:02}:{:02}", secs / 60, secs % 60), "Stop", argb(if on { 255 } else { 80 }, 0xE5, 0x48, 0x4D))
            }
        };
        c.circle(px(22.0) as f32, h as f32 / 2.0, px(6.0) as f32, dot);

        let button = RECT { left: w - px(96.0), top: px(8.0), right: w - px(8.0), bottom: h - px(8.0) };
        c.panel(button, px(6.0) as f32, argb(255, 0x3A, 0x3A, 0x3A), argb(255, 0x4A, 0x4A, 0x4A));
        let text_rect = RECT { left: px(38.0), top: px(4.0), right: button.left - px(8.0), bottom: h - px(4.0) };
        c.text(text_rect, &text, px(16.0), true, rgb(255, 255, 255), DT_VCENTER | DT_SINGLELINE);
        let mut label_rect = RECT { left: button.left + px(2.0), top: button.top + px(2.0), right: button.right - px(2.0), bottom: button.bottom - px(2.0) };
        if s.countdown_end.is_none() {
            // A stop square before the label.
            let (tw, _) = c.text_size(label, px(14.0), true);
            let sq = px(10.0);
            let total = sq + px(8.0) + tw;
            let x0 = button.left + (button.right - button.left - total) / 2;
            let cy = (button.top + button.bottom) / 2;
            c.round_rect(RECT { left: x0, top: cy - sq / 2, right: x0 + sq, bottom: cy + sq / 2 }, px(2.0) as f32, argb(255, 255, 255, 255));
            label_rect.left = x0 + sq + px(8.0);
            c.text(label_rect, label, px(14.0), true, rgb(255, 255, 255), DT_VCENTER | DT_SINGLELINE);
        } else {
            c.text(label_rect, label, px(14.0), true, rgb(255, 255, 255), DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        c.present(s.pill, s.pill_pos);
        s.stop_button = button;
    });
}
