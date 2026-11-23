//! Snipping-Tool-style region picker, in two steps. Step 1: drag an area, click a screen for all of it, or
//! switch the toolbar to Window or Display. Step 2: the selection stays, with handles to move and resize it,
//! and the toolbar beside it holds this recording's options and Record. The result is posted to the owner
//! window as `WM_APP_SELECTED` with a boxed `Outcome`.
//!
//! For a screenshot the screen is frozen first (`screenshot::grab`) and there is no step 2: releasing the
//! drag, or clicking a window or display, takes it. The toolbar switches between the two in place; all the
//! picker's windows are hidden from capture, so the screen can be frozen while it's open.
//!
//! The dim layer is a color-keyed layered window per monitor (one constant alpha; the selection is a
//! keyed-out hole), repainted only where the hole changes and right away rather than at the next WM_PAINT.
//! When frozen, the same window is made opaque and paints a dimmed copy of the grab with the hole at full
//! brightness. Everything drawn on top (outline, handles, size label, hint, toolbar) is a separate
//! per-pixel-alpha window, so it stays opaque and crisp instead of being dimmed along with the screen.

use crate::displays::{CaptureRegion, Display};
use crate::layered::{Canvas, argb};
use crate::screenshot::{Cursor, Frame, Grab, Image};
use crate::settings::Settings;
use crate::win::*;
use std::cell::RefCell;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// Pixels painted this color are fully transparent (the selection "hole").
const KEY: COLORREF = rgb(255, 0, 254);
const DIM_ALPHA: u8 = 150;
const CLASS: PCWSTR = w!("ClipikonOverlay");
const LAYER_CLASS: PCWSTR = w!("ClipikonOverlayLayer");
const TOOLBAR_CLASS: PCWSTR = w!("ClipikonToolbar");
/// Sent after TrackMouseEvent(TME_LEAVE) when the pointer leaves the toolbar (not in the bindings' module).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// NVENC's minimum useful frame, as in `CaptureRegion::from_drag`.
const MIN_W: i32 = 160;
const MIN_H: i32 = 96;

/// What the user picked: the area, and the options for this recording (from the toolbar).
pub struct Selection {
    pub region: CaptureRegion,
    pub settings: Settings,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Video,
    Screenshot,
}

/// How the picker ended. `settings` are the toolbar's choices, for this capture only.
pub enum Outcome {
    Cancelled,
    Record(Selection),
    /// `image` is the area cut from the frozen screen, or None with a delay: it is taken live once that's over.
    Screenshot { region: CaptureRegion, image: Option<Image>, settings: Settings },
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Region,
    Window,
    Display,
}

#[derive(Clone, Copy)]
enum Drag {
    /// Drawing a new selection from this corner.
    New(POINT),
    Move { grab: POINT, orig: RECT },
    /// Edges being dragged: -1 = left/top, 1 = right/bottom, 0 = not on this axis.
    Resize { sx: i8, sy: i8, orig: RECT },
}

#[derive(Clone, Copy, PartialEq)]
enum Item {
    Kind(Kind),
    Mode(Mode),
    Audio,
    Mic,
    Cursor,
    Fps,
    Delay,
    Record,
    Cancel,
}

struct Overlay {
    hwnd: HWND,
    display: Display,
    /// The hole as currently painted, in display-local pixels.
    hole: Option<RECT>,
    /// Screenshot: the frozen display, as grabbed and dimmed.
    frozen: Option<(Frame, Frame)>,
}

/// A per-pixel-alpha window drawn from a canvas (click-through unless it's the toolbar).
struct Layer {
    hwnd: HWND,
    canvas: Option<Canvas>,
    visible: bool,
    pos: POINT,
}

impl Layer {
    fn new(class: PCWSTR, click_through: bool) -> Self {
        let mut ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if click_through {
            ex |= WS_EX_TRANSPARENT;
        }
        let hwnd = create_window(ex, class, WS_POPUP, RECT::default());
        crate::frame::exclude_from_capture(hwnd);
        Self { hwnd, canvas: None, visible: false, pos: POINT::default() }
    }

    /// Draws a `r`-sized canvas and shows it at `r` (screen coordinates), above the overlays.
    fn show(&mut self, r: RECT, draw: impl FnOnce(&mut Canvas)) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return self.hide();
        }
        if self.canvas.as_ref().is_none_or(|c| (c.w, c.h) != (w, h)) {
            self.canvas = Canvas::new(w, h);
        }
        let Some(c) = self.canvas.as_mut() else { return };
        c.clear();
        draw(c);
        self.pos = POINT { x: r.left, y: r.top };
        c.present(self.hwnd, self.pos);
        if !self.visible {
            unsafe {
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            self.visible = true;
        }
    }

    fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }
}

struct Session {
    owner: HWND,
    overlays: Vec<Overlay>,
    /// Selection outline: top, bottom, left, right.
    edges: [Layer; 4],
    handles: [Layer; 8],
    size_label: Layer,
    hint: Layer,
    toolbar: Layer,
    tip: Layer,
    /// Toolbar items and their rects, in toolbar-local pixels.
    items: Vec<(Item, RECT)>,
    hover: Option<Item>,
    settings: Settings,
    kind: Kind,
    /// Screenshot: the mouse cursor as it was when the screen was frozen.
    mouse: Option<Cursor>,
    mode: Mode,
    /// Display (index into `overlays`) with the selection, or under the cursor while there's none.
    active: usize,
    /// The selection, in the active display's local pixels.
    sel: Option<RECT>,
    /// Window mode: the window under the cursor, as a candidate selection.
    preview: Option<RECT>,
    drag: Option<Drag>,
    cursor: POINT,
    accent: COLORREF,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSION.with(|s| s.borrow_mut().as_mut().map(f))
}

/// Starts the picker; `settings` are the options it starts from (and returns, as changed on the toolbar).
/// With `frozen` (one frame per display) it picks a screenshot of that frozen screen.
pub fn select(owner: HWND, displays: &[Display], settings: &Settings, frozen: Option<Grab>) {
    register_class(CLASS, wnd_proc, IDC_CROSS);
    register_class(LAYER_CLASS, layer_proc, IDC_CROSS);
    register_class(TOOLBAR_CLASS, toolbar_proc, IDC_ARROW);
    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let kind = if frozen.is_some() { Kind::Screenshot } else { Kind::Video };
    let (mut frames, mouse) = match frozen {
        Some(g) => (g.frames.into_iter().map(Some).collect(), g.cursor),
        None => (Vec::new(), None),
    };
    frames.resize_with(displays.len(), || None);
    let mut overlays = Vec::new();
    for (d, frame) in displays.iter().zip(frames) {
        let r = RECT { left: d.left, top: d.top, right: d.left + d.width, bottom: d.top + d.height };
        let hwnd = create_window(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW, CLASS, WS_POPUP, r);
        crate::frame::exclude_from_capture(hwnd);
        unsafe {
            // Invisible until its first paint is done (below), so it never shows a blank frame.
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        overlays.push(Overlay { hwnd, display: d.clone(), hole: None, frozen: dimmed_pair(frame) });
    }
    if overlays.is_empty() {
        return;
    }
    let active = overlays.iter().position(|o| o.display.contains(cursor.x, cursor.y)).unwrap_or(0);
    let focus = overlays[active].hwnd;
    let accent = crate::fluent::system_accent();
    let layer = || Layer::new(LAYER_CLASS, true);
    let session = Session {
        owner,
        overlays,
        edges: std::array::from_fn(|_| layer()),
        handles: std::array::from_fn(|_| layer()),
        size_label: layer(),
        hint: layer(),
        toolbar: Layer::new(TOOLBAR_CLASS, false),
        tip: layer(),
        items: Vec::new(),
        hover: None,
        settings: settings.clone(),
        kind,
        mouse,
        mode: Mode::Region,
        active,
        sel: None,
        preview: None,
        drag: None,
        cursor,
        accent: rgb(accent.r(), accent.g(), accent.b()),
    };
    SESSION.with(|s| *s.borrow_mut() = Some(session));
    unsafe {
        // The hotkey grants us foreground rights, so this works even over a focused game. Before the layers
        // are shown: activating an overlay raises it, and it would then cover them.
        let _ = SetForegroundWindow(focus);
        let _ = SetFocus(Some(focus));
    }
    refresh();
    for (hwnd, frozen) in overlay_looks() {
        paint_now(hwnd);
        set_look(hwnd, frozen);
    }
}

/// A grabbed display and its dimmed copy (the same darkening the live overlay gives the screen).
fn dimmed_pair(frame: Option<Frame>) -> Option<(Frame, Frame)> {
    frame.and_then(|f| Some((f.dimmed(255 - DIM_ALPHA)?, f))).map(|(dim, f)| (f, dim))
}

/// The overlay is always a layered window; only how it's blended changes. Frozen: opaque, painting the dimmed
/// grab with the hole at full brightness. Live: the black it paints is blended at `DIM_ALPHA` over the screen
/// and the hole, painted in `KEY`, is cut out.
fn set_look(hwnd: HWND, frozen: bool) {
    unsafe {
        let _ = if frozen {
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)
        } else {
            SetLayeredWindowAttributes(hwnd, KEY, DIM_ALPHA, LWA_COLORKEY | LWA_ALPHA)
        };
    }
}

fn paint_now(hwnd: HWND) {
    unsafe {
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW);
    }
}

/// Each overlay and whether it's frozen. Painting reads the session, so it happens outside its borrow.
fn overlay_looks() -> Vec<(HWND, bool)> {
    with(|s| s.overlays.iter().map(|o| (o.hwnd, o.frozen.is_some())).collect()).unwrap_or_default()
}

/// The toolbar's camera/video buttons: turns the open picker into the other kind. A screenshot freezes the
/// screen as it is under the picker (whose windows capture can't see).
fn switch_kind(kind: Kind) {
    let Some(displays) = with(|s| (s.kind != kind).then(|| s.overlays.iter().map(|o| o.display.clone()).collect::<Vec<_>>())).flatten()
    else {
        return;
    };
    let grab = match kind {
        Kind::Screenshot => match crate::screenshot::grab(&displays) {
            Some(g) => Some(g),
            None => return, // couldn't capture: stay as is
        },
        Kind::Video => None,
    };
    with(|s| {
        let (mut frames, mouse) = match grab {
            Some(g) => (g.frames.into_iter().map(Some).collect(), g.cursor),
            None => (Vec::new(), None),
        };
        frames.resize_with(s.overlays.len(), || None);
        for (o, f) in s.overlays.iter_mut().zip(frames) {
            o.frozen = dimmed_pair(f);
        }
        s.kind = kind;
        s.mouse = mouse;
        s.drag = None;
        // Carry the mode over: a screenshot has no settled selection, a recording's Display mode selects one.
        let d = s.overlays[s.active].display.clone();
        s.sel = None;
        s.preview = None;
        match (kind, s.mode) {
            (Kind::Screenshot, Mode::Display) => s.preview = Some(whole(&d)),
            (Kind::Video, Mode::Display) => s.sel = Some(full(&d)),
            _ => {}
        }
    });
    match kind {
        // The frozen look is painted first (at the live blend for a moment it looks just like the live screen),
        // then made opaque.
        Kind::Screenshot => {
            refresh();
            for (hwnd, _) in overlay_looks() {
                paint_now(hwnd);
                set_look(hwnd, true);
            }
        }
        // Changing the blend drops what the window showed until it repaints, so the change and the repaint go
        // right after a composition, to land in the same next frame; the slower layers (toolbar) come after.
        Kind::Video => {
            update_holes();
            unsafe {
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
            }
            for (hwnd, _) in overlay_looks() {
                set_look(hwnd, false);
                paint_now(hwnd);
            }
            refresh();
        }
    }
}

/// Puts the visible layers back above the overlays (an overlay that gets activated, e.g. by a click, is
/// raised to the top of the topmost windows and would cover them).
fn raise_layers() {
    with(|s| {
        let layers = s.edges.iter().chain(&s.handles).chain([&s.size_label, &s.hint, &s.toolbar, &s.tip]);
        for layer in layers.filter(|l| l.visible) {
            unsafe {
                let _ = SetWindowPos(layer.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    });
}

fn finish(result: Outcome) {
    let Some(s) = SESSION.with(|s| s.borrow_mut().take()) else { return };
    unsafe {
        let layers = s.edges.iter().chain(&s.handles).chain([&s.size_label, &s.hint, &s.toolbar, &s.tip]);
        for layer in layers {
            let _ = DestroyWindow(layer.hwnd);
        }
        for o in &s.overlays {
            let _ = DestroyWindow(o.hwnd);
        }
        let boxed = Box::into_raw(Box::new(result));
        if PostMessageW(Some(s.owner), WM_APP_SELECTED, WPARAM(0), LPARAM(boxed as isize)).is_err() {
            drop(Box::from_raw(boxed));
        }
    }
}

fn record() {
    let picked = with(|s| {
        let r = s.sel.filter(|_| s.kind == Kind::Video)?;
        let d = &s.overlays[s.active].display;
        Some(Selection { region: CaptureRegion::from_drag(d, r.left, r.top, r.right, r.bottom), settings: s.settings.clone() })
    })
    .flatten();
    if let Some(p) = picked {
        finish(Outcome::Record(p));
    }
}

/// Takes the screenshot of `r` (the active display's pixels): cut from the frozen screen, with the cursor
/// if that's on, or left for later with a delay.
fn shoot(r: RECT) {
    let outcome = with(|s| {
        let o = &s.overlays[s.active];
        let d = o.display.clone();
        let region = CaptureRegion { display: d.clone(), x: r.left, y: r.top, width: r.right - r.left, height: r.bottom - r.top };
        let image = if s.settings.screenshot_delay_secs > 0 {
            None
        } else {
            o.frozen.as_ref().and_then(|(frame, _)| {
                let cut = frame.crop(r)?;
                if s.settings.screenshot_cursor
                    && let Some(m) = &s.mouse
                {
                    m.draw(&cut, POINT { x: d.left + r.left, y: d.top + r.top });
                }
                Some(cut.to_image())
            })
        };
        Outcome::Screenshot { region, image, settings: s.settings.clone() }
    });
    if let Some(o) = outcome {
        finish(o);
    }
}

/// A whole display, as recorded (even size, for the encoder).
fn full(d: &Display) -> RECT {
    RECT { left: 0, top: 0, right: d.width & !1, bottom: d.height & !1 }
}

/// A whole display, to the last pixel.
fn whole(d: &Display) -> RECT {
    RECT { left: 0, top: 0, right: d.width, bottom: d.height }
}

/// What a drag (display-local) turns into: for a recording, as `as_recorded`; for a screenshot, the rect as
/// drawn, or the whole display for a click.
fn fitted(kind: Kind, d: &Display, r: RECT) -> RECT {
    match kind {
        Kind::Video => as_recorded(d, r),
        Kind::Screenshot if r.right - r.left < 4 && r.bottom - r.top < 4 => whole(d),
        Kind::Screenshot => RECT { left: r.left.max(0), top: r.top.max(0), right: r.right.min(d.width), bottom: r.bottom.min(d.height) },
    }
}

fn normalized(a: POINT, b: POINT) -> RECT {
    RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x), bottom: a.y.max(b.y) }
}

/// The rect as `CaptureRegion::from_drag` would record it (minimum size, even, on screen; a click = all).
fn as_recorded(d: &Display, r: RECT) -> RECT {
    let c = CaptureRegion::from_drag(d, r.left, r.top, r.right, r.bottom);
    RECT { left: c.x, top: c.y, right: c.x + c.width, bottom: c.y + c.height }
}

fn scale_of(d: &Display) -> f64 {
    scale_at(d.left + d.width / 2, d.top + d.height / 2)
}

/// Which part of selection `r` is at `p`: `Some((0, 0))` inside (move), `Some((sx, sy))` on an edge or
/// corner (resize), `None` outside.
fn hit(r: RECT, p: POINT, margin: i32) -> Option<(i8, i8)> {
    if p.x < r.left - margin || p.x > r.right + margin || p.y < r.top - margin || p.y > r.bottom + margin {
        return None;
    }
    let near = |v: i32, edge: i32| (v - edge).abs() <= margin;
    let sx = if near(p.x, r.left) { -1 } else if near(p.x, r.right) { 1 } else { 0 };
    let sy = if near(p.y, r.top) { -1 } else if near(p.y, r.bottom) { 1 } else { 0 };
    let inside = p.x > r.left && p.x < r.right && p.y > r.top && p.y < r.bottom;
    ((sx, sy) != (0, 0) || inside).then_some((sx, sy))
}

fn drag_rect(drag: Drag, p: POINT, d: &Display) -> RECT {
    match drag {
        Drag::New(a) => normalized(a, p),
        Drag::Move { grab, orig } => {
            let dx = (p.x - grab.x).clamp(-orig.left, d.width - orig.right);
            let dy = (p.y - grab.y).clamp(-orig.top, d.height - orig.bottom);
            RECT { left: orig.left + dx, top: orig.top + dy, right: orig.right + dx, bottom: orig.bottom + dy }
        }
        Drag::Resize { sx, sy, orig } => {
            let mut r = orig;
            match sx {
                -1 => r.left = p.x.clamp(0, r.right - MIN_W),
                1 => r.right = p.x.clamp(r.left + MIN_W, d.width),
                _ => {}
            }
            match sy {
                -1 => r.top = p.y.clamp(0, r.bottom - MIN_H),
                1 => r.bottom = p.y.clamp(r.top + MIN_H, d.height),
                _ => {}
            }
            r
        }
    }
}

/// Topmost visible window (not ours) under screen point `p`, by its visible frame.
fn window_at(p: POINT) -> Option<RECT> {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        unsafe { (*(lp.0 as *mut Vec<HWND>)).push(hwnd) };
        true.into()
    }
    let mut list: Vec<HWND> = Vec::new();
    let me = std::process::id();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
        for hwnd in list {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if pid == me || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || ex & WS_EX_TRANSPARENT.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
            if cloaked != 0 {
                continue;
            }
            let mut class = [0u16; 32];
            let n = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            if class == "Progman" || class == "WorkerW" || class == "Shell_TrayWnd" {
                continue; // the desktop and taskbar: not windows to record
            }
            let mut r = RECT::default();
            if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, size_of::<RECT>() as u32).is_err() {
                let _ = GetWindowRect(hwnd, &mut r);
            }
            if r.right - r.left > 8 && r.bottom - r.top > 8 && p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom {
                return Some(r);
            }
        }
    }
    None
}

/// The window under the cursor, clipped to display `d`, in its local pixels.
fn window_preview(d: &Display, p: POINT) -> Option<RECT> {
    let r = window_at(p)?;
    let clip = RECT {
        left: (r.left - d.left).max(0),
        top: (r.top - d.top).max(0),
        right: (r.right - d.left).min(d.width),
        bottom: (r.bottom - d.top).min(d.height),
    };
    (clip.right > clip.left && clip.bottom > clip.top).then_some(clip)
}

// ---- drawing ----

/// Records the holes the session state calls for, without painting them.
fn update_holes() {
    with(|s| {
        let (shown, active) = (shown_rect(s), s.active);
        for (i, o) in s.overlays.iter_mut().enumerate() {
            o.hole = if i == active { shown } else { None };
        }
    });
}

/// Brings every window in line with the session state.
fn refresh() {
    // 1. Holes. Painting happens outside the borrow: UpdateWindow sends WM_PAINT, which reads the session.
    let Some(changes) = with(|s| {
        let shown = shown_rect(s);
        let active = s.active;
        s.overlays
            .iter_mut()
            .enumerate()
            .filter_map(|(i, o)| {
                let new = if i == active { shown } else { None };
                let old = std::mem::replace(&mut o.hole, new);
                (old != new).then_some((o.hwnd, old, new))
            })
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    for (hwnd, old, new) in changes {
        repaint_hole(hwnd, old, new);
    }
    // 2. Layers on top.
    with(draw_layers);
}

/// The hole to show: the selection being dragged, the selection, or the window under the cursor.
fn shown_rect(s: &Session) -> Option<RECT> {
    let d = &s.overlays[s.active].display;
    match s.drag {
        Some(drag) => Some(drag_rect(drag, s.cursor, d)),
        None => s.sel.or(s.preview),
    }
}

fn repaint_hole(hwnd: HWND, old: Option<RECT>, new: Option<RECT>) {
    unsafe {
        let rgn = |r: Option<RECT>| r.map_or_else(|| CreateRectRgn(0, 0, 0, 0), |r| CreateRectRgn(r.left, r.top, r.right, r.bottom));
        let (a, b) = (rgn(old), rgn(new));
        let changed = CreateRectRgn(0, 0, 0, 0);
        CombineRgn(Some(changed), Some(a), Some(b), RGN_XOR);
        let _ = InvalidateRgn(hwnd, Some(changed), false);
        let _ = UpdateWindow(hwnd);
        for r in [a, b, changed] {
            let _ = DeleteObject(r.into());
        }
    }
}

fn draw_layers(s: &mut Session) {
    let d = s.overlays[s.active].display.clone();
    let scale = scale_of(&d);
    let px = |v: f64| (v * scale).round() as i32;
    let to_screen = |r: RECT| RECT { left: d.left + r.left, top: d.top + r.top, right: d.left + r.right, bottom: d.top + r.bottom };
    let shown = shown_rect(s).map(to_screen);
    let screen = RECT { left: d.left, top: d.top, right: d.left + d.width, bottom: d.top + d.height };
    let dragging = s.drag.is_some();
    let new_drag = matches!(s.drag, Some(Drag::New(_)));
    let white = argb(255, 255, 255, 255);

    // Outline, just outside the selection, or just inside where the selection meets the screen edge.
    let t = px(2.0).max(2);
    match shown {
        Some(r) => {
            let o = RECT {
                left: (r.left - t).max(screen.left),
                top: (r.top - t).max(screen.top),
                right: (r.right + t).min(screen.right),
                bottom: (r.bottom + t).min(screen.bottom),
            };
            let i = RECT { left: o.left + t, top: o.top + t, right: o.right - t, bottom: o.bottom - t };
            let edges = [
                RECT { left: o.left, top: o.top, right: o.right, bottom: i.top },
                RECT { left: o.left, top: i.bottom, right: o.right, bottom: o.bottom },
                RECT { left: o.left, top: i.top, right: i.left, bottom: i.bottom },
                RECT { left: i.right, top: i.top, right: o.right, bottom: i.bottom },
            ];
            for (layer, e) in s.edges.iter_mut().zip(edges) {
                layer.show(e, |c| c.fill(RECT { left: 0, top: 0, right: c.w, bottom: c.h }, white));
            }
        }
        None => s.edges.iter_mut().for_each(Layer::hide),
    }

    // Handles on a settled (or moving/resizing) selection.
    match (s.sel.map(to_screen).filter(|_| !new_drag), shown) {
        (Some(_), Some(r)) => {
            let hs = px(9.0).max(7);
            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            let mids = r.right - r.left > hs * 4 && r.bottom - r.top > hs * 4;
            let points = [
                (r.left, r.top), (r.right, r.top), (r.left, r.bottom), (r.right, r.bottom),
                (cx, r.top), (cx, r.bottom), (r.left, cy), (r.right, cy),
            ];
            for (i, (layer, (x, y))) in s.handles.iter_mut().zip(points).enumerate() {
                if i >= 4 && !mids {
                    layer.hide();
                    continue;
                }
                // Centered on the edge, but pushed fully onto the screen (a whole-screen selection's handles
                // sit in its corners).
                let x = (x - hs / 2).clamp(screen.left, screen.right - hs);
                let y = (y - hs / 2).clamp(screen.top, screen.bottom - hs);
                let hr = RECT { left: x, top: y, right: x + hs, bottom: y + hs };
                layer.show(hr, |c| c.panel(RECT { left: 0, top: 0, right: c.w, bottom: c.h }, px(2.0) as f32, white, argb(255, 0x20, 0x20, 0x20)));
            }
        }
        _ => s.handles.iter_mut().for_each(Layer::hide),
    }

    // Size label, under the bottom-left corner (the toolbar takes the top).
    match shown {
        Some(r) => {
            let local = RECT { left: r.left - d.left, top: r.top - d.top, right: r.right - d.left, bottom: r.bottom - d.top };
            let rec = if new_drag && s.kind == Kind::Screenshot { local } else { fitted(s.kind, &d, local) };
            let text = format!("{} × {}", rec.right - rec.left, rec.bottom - rec.top);
            let Some(m) = Canvas::new(1, 1) else { return };
            let (tw, th) = m.text_size(&text, px(13.0), true);
            let (padx, pady, gap) = (px(8.0), px(4.0), px(8.0));
            let (w, h) = (tw + padx * 2, th + pady * 2);
            let mut y = r.bottom + t + gap;
            if y + h > d.top + d.height {
                y = r.bottom - gap - h;
            }
            let x = r.left.clamp(d.left, d.left + d.width - w);
            s.size_label.show(RECT { left: x, top: y, right: x + w, bottom: y + h }, |c| {
                c.panel(RECT { left: 0, top: 0, right: w, bottom: h }, px(4.0) as f32, argb(255, 0x2B, 0x2B, 0x2B), argb(255, 0x45, 0x45, 0x45));
                c.text(RECT { left: 0, top: pady, right: w, bottom: h - pady }, &text, px(13.0), true, rgb(255, 255, 255), DT_CENTER | DT_SINGLELINE);
            });
        }
        None => s.size_label.hide(),
    }

    // Toolbar: next to the selection in step 2, at the top of the screen in step 1, hidden while dragging.
    if dragging {
        s.toolbar.hide();
        s.tip.hide();
        s.hint.hide();
        return;
    }
    let (w, h) = draw_toolbar(s, scale, None);
    let gap = px(12.0);
    let (x, y) = match s.sel.map(to_screen) {
        Some(r) => {
            let x = ((r.left + r.right) / 2 - w / 2).clamp(d.left + gap, d.left + d.width - w - gap);
            let mut y = r.top - t - gap - h;
            if y < d.top + gap {
                y = r.bottom + t + gap + px(36.0); // below, clear of the size label
            }
            if y + h > d.top + d.height - gap {
                y = r.top + gap; // no room outside: inside the top edge
            }
            (x, y)
        }
        None => (d.left + (d.width - w) / 2, d.top + px(24.0)),
    };
    let tb = RECT { left: x, top: y, right: x + w, bottom: y + h };
    draw_toolbar(s, scale, Some(tb));

    // Hint under the toolbar while nothing is selected.
    if s.sel.is_none() {
        let text = match (s.kind, s.mode) {
            (Kind::Video, Mode::Window) => "Click a window to select it  ·  Esc to cancel",
            (Kind::Video, _) => "Drag to select an area  ·  Click to select the whole screen  ·  Esc to cancel",
            (Kind::Screenshot, Mode::Region) => "Drag to capture an area  ·  Click to capture the whole screen  ·  Esc to cancel",
            (Kind::Screenshot, Mode::Window) => "Click a window to capture it  ·  Esc to cancel",
            (Kind::Screenshot, Mode::Display) => "Click a screen to capture it  ·  Esc to cancel",
        };
        let Some(m) = Canvas::new(1, 1) else { return };
        let (tw, th) = m.text_size(text, px(14.0), false);
        let (padx, pady) = (px(14.0), px(7.0));
        let (hw, hh) = (tw + padx * 2, th + pady * 2);
        let hx = d.left + (d.width - hw) / 2;
        let hy = tb.bottom + px(8.0);
        s.hint.show(RECT { left: hx, top: hy, right: hx + hw, bottom: hy + hh }, |c| {
            c.panel(RECT { left: 0, top: 0, right: hw, bottom: hh }, px(6.0) as f32, argb(255, 0x2B, 0x2B, 0x2B), argb(255, 0x45, 0x45, 0x45));
            c.text(RECT { left: padx, top: pady, right: hw - padx, bottom: hh - pady }, text, px(14.0), false, rgb(0xE0, 0xE0, 0xE0), DT_CENTER | DT_SINGLELINE);
        });
    } else {
        s.hint.hide();
    }

    // Tooltip under the hovered toolbar item.
    match s.hover.and_then(|i| s.items.iter().find(|(it, _)| *it == i).copied()) {
        Some((item, ir)) => {
            let text = tooltip(item, &s.settings, s.kind);
            let Some(m) = Canvas::new(1, 1) else { return };
            let (tw, th) = m.text_size(&text, px(12.0), false);
            let (padx, pady) = (px(8.0), px(4.0));
            let (ww, hh) = (tw + padx * 2, th + pady * 2);
            let tx = (tb.left + (ir.left + ir.right) / 2 - ww / 2).clamp(d.left, d.left + d.width - ww);
            let ty = tb.bottom + px(6.0);
            s.tip.show(RECT { left: tx, top: ty, right: tx + ww, bottom: ty + hh }, |c| {
                c.panel(RECT { left: 0, top: 0, right: ww, bottom: hh }, px(4.0) as f32, argb(255, 0x2B, 0x2B, 0x2B), argb(255, 0x45, 0x45, 0x45));
                c.text(RECT { left: 0, top: pady, right: ww, bottom: hh - pady }, &text, px(12.0), false, rgb(0xE0, 0xE0, 0xE0), DT_CENTER | DT_SINGLELINE);
            });
        }
        None => s.tip.hide(),
    }
}

fn fps_label(fps: u32, hz: u32) -> String {
    match (fps, hz) {
        (0, 0) => "Max fps".into(),
        (0, hz) => format!("{hz} fps"),
        (f, _) => format!("{f} fps"),
    }
}

fn delay_label(secs: u32) -> String {
    if secs == 0 { "No delay".into() } else { format!("{secs} s delay") }
}

/// The cursor and delay options of `kind`: screenshots keep their own.
fn cursor_on(s: &Settings, kind: Kind) -> bool {
    if kind == Kind::Screenshot { s.screenshot_cursor } else { s.draw_mouse }
}

fn delay_of(s: &Settings, kind: Kind) -> u32 {
    if kind == Kind::Screenshot { s.screenshot_delay_secs } else { s.start_delay_secs }
}

fn tooltip(item: Item, s: &Settings, kind: Kind) -> String {
    let on_off = |b: bool| if b { "on" } else { "off" };
    match item {
        Item::Kind(Kind::Screenshot) => format!("Screenshot ({})", s.screenshot_hotkey),
        Item::Kind(Kind::Video) => format!("Record video ({})", s.hotkey),
        Item::Mode(Mode::Region) => "Select an area".into(),
        Item::Mode(Mode::Window) => "Select a window".into(),
        Item::Mode(Mode::Display) => "Whole screen".into(),
        Item::Audio => format!("System audio: {}", on_off(s.capture_system_audio)),
        Item::Mic => "Microphone recording is coming soon".into(),
        Item::Cursor => format!("Mouse cursor: {}", on_off(cursor_on(s, kind))),
        Item::Fps => "Frame rate".into(),
        Item::Delay if kind == Kind::Screenshot => "Wait before taking the screenshot".into(),
        Item::Delay => "Wait before recording starts".into(),
        Item::Record => "Start recording (Enter)".into(),
        Item::Cancel => "Cancel (Esc)".into(),
    }
}

/// Lays out the toolbar and returns its size; with `at`, also draws it there and stores the item rects.
fn draw_toolbar(s: &mut Session, scale: f64, at: Option<RECT>) -> (i32, i32) {
    let px = |v: f64| (v * scale).round() as i32;
    let Some(m) = Canvas::new(1, 1) else { return (0, 0) };
    let hz = s.overlays[s.active].display.refresh_hz;
    let (h, ih, pad) = (px(48.0), px(38.0), px(5.0));
    let text_px = px(14.0);
    let glyph_px = px(16.0);
    let kind = s.kind;
    let fps = fps_label(s.settings.fps, hz);
    let delay = delay_label(delay_of(&s.settings, kind));

    // (item, width, separator before it)
    let text_w = |t: &str, bold: bool| m.text_size(t, text_px, bold).0;
    let seg = |t: &str| px(12.0) + glyph_px + px(8.0) + text_w(t, false) + px(12.0);
    let drop = |t: &str| px(12.0) + text_w(t, false) + px(8.0) + px(12.0) + px(10.0);
    let square = px(38.0);
    let mut layout: Vec<(Item, i32, bool)> = vec![
        (Item::Kind(Kind::Screenshot), square, false),
        (Item::Kind(Kind::Video), square, false),
        (Item::Mode(Mode::Region), seg("Region"), true),
        (Item::Mode(Mode::Window), seg("Window"), false),
        (Item::Mode(Mode::Display), seg("Display"), false),
    ];
    match kind {
        Kind::Video => layout.extend([
            (Item::Audio, square, true),
            (Item::Mic, square, false),
            (Item::Cursor, square, false),
            (Item::Fps, drop(&fps), false),
            (Item::Delay, drop(&delay), false),
            (Item::Record, px(14.0) + px(10.0) + px(8.0) + text_w("Record", true) + px(14.0), true),
            (Item::Cancel, square, false),
        ]),
        // A screenshot is taken as soon as the area is picked: no button for it.
        Kind::Screenshot => layout.extend([(Item::Cursor, square, true), (Item::Delay, drop(&delay), false), (Item::Cancel, square, true)]),
    }
    let mut items = Vec::new();
    let mut x = pad;
    for (item, w, sep) in &layout {
        if *sep {
            x += px(9.0);
        }
        items.push((*item, RECT { left: x, top: (h - ih) / 2, right: x + w, bottom: (h - ih) / 2 + ih }));
        x += w + px(2.0);
    }
    let w = x - px(2.0) + pad;
    let Some(at) = at else { return (w, h) };

    let (accent, hover, settings, mode, has_sel) = (s.accent, s.hover, s.settings.clone(), s.mode, s.sel.is_some());
    let separators: Vec<i32> = items.iter().zip(&layout).filter(|(_, l)| l.2).map(|((_, r), _)| r.left - px(5.0)).collect();
    s.toolbar.show(at, |c| {
        c.panel(RECT { left: 0, top: 0, right: w, bottom: h }, px(8.0) as f32, argb(255, 0x2B, 0x2B, 0x2B), argb(255, 0x45, 0x45, 0x45));
        for sx in &separators {
            c.fill(RECT { left: *sx, top: px(12.0), right: sx + 1, bottom: h - px(12.0) }, argb(255, 0x45, 0x45, 0x45));
        }
        let white = rgb(255, 255, 255);
        let dim = rgb(0xC5, 0xC5, 0xC5);
        let disabled = rgb(0x6E, 0x6E, 0x6E);
        let accent_argb = argb(255, accent.0 as u8, (accent.0 >> 8) as u8, (accent.0 >> 16) as u8);
        for (item, r) in &items {
            let hovered = hover == Some(*item);
            let enabled = !matches!(item, Item::Mic) && (*item != Item::Record || has_sel);
            let selected = *item == Item::Mode(mode) || *item == Item::Kind(kind);
            let bg = match item {
                Item::Record if enabled => Some(if hovered { argb(255, 0xD6, 0x3A, 0x2C) } else { argb(255, 0xC4, 0x2B, 0x1C) }),
                Item::Record => Some(argb(255, 0x3A, 0x3A, 0x3A)),
                _ if selected => Some(argb(255, 0x40, 0x40, 0x40)),
                _ if hovered && enabled => Some(argb(255, 0x36, 0x36, 0x36)),
                _ => None,
            };
            if let Some(bg) = bg {
                c.round_rect(*r, px(5.0) as f32, bg);
            }
            let fg = if !enabled { disabled } else if selected { white } else { dim };
            let glyph_rect = |left: i32| RECT { left, top: r.top, right: left + glyph_px + px(4.0), bottom: r.bottom };
            match item {
                Item::Kind(k) => {
                    let g = if *k == Kind::Screenshot { "\u{E722}" } else { "\u{E714}" }; // camera, video
                    c.glyph(*r, g, glyph_px, fg);
                }
                Item::Mode(m) => {
                    let (g, label) = match m {
                        Mode::Region => ("\u{E8B3}", "Region"),
                        Mode::Window => ("\u{E737}", "Window"),
                        Mode::Display => ("\u{E7F4}", "Display"),
                    };
                    c.glyph(glyph_rect(r.left + px(10.0)), g, glyph_px, fg);
                    let tx = RECT { left: r.left + px(12.0) + glyph_px + px(8.0), top: r.top, right: r.right, bottom: r.bottom };
                    c.text(tx, label, text_px, selected, fg, DT_VCENTER | DT_SINGLELINE);
                    if selected {
                        let bar = RECT { left: (r.left + r.right) / 2 - px(8.0), top: r.bottom - px(4.0), right: (r.left + r.right) / 2 + px(8.0), bottom: r.bottom - px(1.0) };
                        c.round_rect(bar, px(1.5) as f32, accent_argb);
                    }
                }
                Item::Audio | Item::Mic | Item::Cursor => {
                    let (g, on) = match item {
                        Item::Audio if settings.capture_system_audio => ("\u{E767}", true),
                        Item::Audio => ("\u{E74F}", false),
                        Item::Mic => ("\u{E720}", false),
                        _ => ("\u{E8B0}", cursor_on(&settings, kind)),
                    };
                    let color = if !enabled { disabled } else if on { accent } else { dim };
                    c.glyph(*r, g, glyph_px, color);
                }
                Item::Fps | Item::Delay => {
                    let label = if *item == Item::Fps { &fps } else { &delay };
                    let tx = RECT { left: r.left + px(12.0), top: r.top, right: r.right - px(24.0), bottom: r.bottom };
                    c.text(tx, label, text_px, false, white, DT_VCENTER | DT_SINGLELINE);
                    c.glyph(RECT { left: r.right - px(26.0), top: r.top, right: r.right - px(8.0), bottom: r.bottom }, "\u{E70D}", px(11.0), dim);
                }
                Item::Record => {
                    let cy = (r.top + r.bottom) as f32 / 2.0;
                    c.circle((r.left + px(14.0) + px(5.0)) as f32, cy, px(5.0) as f32, if enabled { argb(255, 255, 255, 255) } else { argb(255, 0x6E, 0x6E, 0x6E) });
                    let tx = RECT { left: r.left + px(14.0) + px(10.0) + px(8.0), top: r.top, right: r.right, bottom: r.bottom };
                    c.text(tx, "Record", text_px, true, if enabled { white } else { disabled }, DT_VCENTER | DT_SINGLELINE);
                }
                Item::Cancel => c.glyph(*r, "\u{E711}", px(13.0), dim),
            }
        }
    });
    s.items = items;
    (w, h)
}

// ---- input ----

fn overlay_index(s: &Session, hwnd: HWND) -> Option<usize> {
    s.overlays.iter().position(|o| o.hwnd == hwnd)
}

fn local(d: &Display) -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    POINT { x: (p.x - d.left).clamp(0, d.width), y: (p.y - d.top).clamp(0, d.height) }
}

fn on_mouse_down(hwnd: HWND, lp: LPARAM) {
    // A screenshot of a window or display is taken right on the click.
    let instant = with(|s| {
        let i = overlay_index(s, hwnd)?;
        if s.kind != Kind::Screenshot || s.mode == Mode::Region {
            return None;
        }
        let d = s.overlays[i].display.clone();
        s.active = i;
        let mut screen = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut screen);
        }
        Some(match s.mode {
            Mode::Window => window_preview(&d, screen).unwrap_or_else(|| whole(&d)),
            _ => whole(&d),
        })
    })
    .flatten();
    if let Some(r) = instant {
        return shoot(r);
    }
    let capture = with(|s| {
        let i = overlay_index(s, hwnd)?;
        let d = s.overlays[i].display.clone();
        // Where the button went down (the overlay's client area is the display), not where the cursor is
        // by the time this is handled: a quick grab of an edge would otherwise miss it.
        let p = POINT { x: (lp.0 as i16 as i32).clamp(0, d.width), y: ((lp.0 >> 16) as i16 as i32).clamp(0, d.height) };
        s.cursor = p;
        // On the selection, in any mode: move or resize it.
        let margin = (10.0 * scale_of(&d)) as i32;
        if let Some((orig, (sx, sy))) = s.sel.filter(|_| s.active == i).and_then(|r| Some((r, hit(r, p, margin)?))) {
            s.drag = Some(if (sx, sy) == (0, 0) { Drag::Move { grab: p, orig } } else { Drag::Resize { sx, sy, orig } });
            return Some(());
        }
        s.active = i;
        match s.mode {
            Mode::Window => {
                let screen = POINT { x: d.left + p.x, y: d.top + p.y };
                s.sel = Some(window_preview(&d, screen).unwrap_or_else(|| full(&d)));
                s.preview = None;
                None
            }
            Mode::Display => {
                s.sel = Some(full(&d));
                None
            }
            Mode::Region => {
                s.sel = None;
                s.drag = Some(Drag::New(p));
                Some(())
            }
        }
    })
    .flatten();
    if capture.is_some() {
        unsafe {
            SetCapture(hwnd);
        }
    }
    refresh();
}

fn on_mouse_move(hwnd: HWND) {
    let changed = with(|s| {
        let i = overlay_index(s, hwnd)?;
        let d = s.overlays[i].display.clone();
        if s.drag.is_some() {
            s.cursor = local(&s.overlays[s.active].display);
            return Some(true);
        }
        let p = local(&d);
        s.cursor = p;
        let mut changed = false;
        if s.sel.is_none() && s.active != i {
            s.active = i; // the toolbar follows the cursor across monitors
            changed = true;
        }
        if s.mode == Mode::Window && s.sel.is_none() {
            let mut screen = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut screen);
            }
            let preview = window_preview(&d, screen);
            changed |= preview != s.preview;
            s.preview = preview;
        }
        if s.kind == Kind::Screenshot && s.mode == Mode::Display {
            // The screen that a click would capture.
            changed |= s.preview != Some(whole(&d));
            s.preview = Some(whole(&d));
        }
        if s.hover.take().is_some() {
            changed = true;
        }
        Some(changed)
    })
    .flatten();
    if changed == Some(true) {
        refresh();
    }
}

fn on_mouse_up() {
    let shot = with(|s| {
        let Some(drag) = s.drag.take() else { return None };
        let d = s.overlays[s.active].display.clone();
        let r = drag_rect(drag, s.cursor, &d);
        if s.kind == Kind::Screenshot {
            return Some(fitted(s.kind, &d, r));
        }
        // A click (no real drag) selects the whole screen; tiny drags grow to the minimum.
        // A resized or moved window/display pick is now just an area.
        if let Drag::Move { orig, .. } | Drag::Resize { orig, .. } = drag
            && orig != r
        {
            s.mode = Mode::Region;
        }
        s.sel = Some(as_recorded(&d, r));
        None
    })
    .flatten();
    unsafe {
        let _ = ReleaseCapture();
    }
    match shot {
        Some(r) => shoot(r),
        None => refresh(),
    }
}

fn cursor_for(hwnd: HWND) -> PCWSTR {
    with(|s| {
        let i = overlay_index(s, hwnd)?;
        let d = &s.overlays[i].display;
        if (s.mode == Mode::Window && s.sel.is_none()) || (s.kind == Kind::Screenshot && s.mode == Mode::Display) {
            return Some(IDC_HAND);
        }
        let drag = match s.drag {
            Some(Drag::Move { .. }) => Some((0, 0)),
            Some(Drag::Resize { sx, sy, .. }) => Some((sx, sy)),
            Some(Drag::New(_)) => return Some(IDC_CROSS),
            None => s.sel.filter(|_| s.active == i).and_then(|r| hit(r, local(d), (10.0 * scale_of(d)) as i32)),
        };
        Some(match drag {
            Some((0, 0)) => IDC_SIZEALL,
            Some((-1, -1)) | Some((1, 1)) => IDC_SIZENWSE,
            Some((1, -1)) | Some((-1, 1)) => IDC_SIZENESW,
            Some((_, 0)) => IDC_SIZEWE,
            Some((0, _)) => IDC_SIZENS,
            _ => IDC_CROSS,
        })
    })
    .flatten()
    .unwrap_or(IDC_CROSS)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => on_mouse_down(hwnd, lp),
        WM_MOUSEMOVE => on_mouse_move(hwnd),
        WM_LBUTTONUP => on_mouse_up(),
        WM_RBUTTONUP => finish(Outcome::Cancelled),
        WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 != WA_INACTIVE => raise_layers(),
        WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => finish(Outcome::Cancelled),
        WM_KEYDOWN if wp.0 as u16 == VK_RETURN.0 => record(),
        WM_SETCURSOR => unsafe {
            SetCursor(LoadCursorW(None, cursor_for(hwnd)).ok());
            return LRESULT(1);
        },
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // Straight to the window DC: the update region is usually a few thin strips and painting is
            // clipped to it, so double buffering would only add a full-screen cost.
            let (hole, frozen) = with(|s| {
                let o = s.overlays.iter().find(|o| o.hwnd == hwnd)?;
                Some((o.hole, o.frozen.as_ref().map(|(bright, dim)| (bright.dc, dim.dc))))
            })
            .flatten()
            .unwrap_or_default();
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let p = ps.rcPaint;
                match frozen {
                    // The frozen screen: dimmed, and at full brightness in the hole.
                    Some((bright, dim)) => {
                        let _ = BitBlt(hdc, p.left, p.top, p.right - p.left, p.bottom - p.top, Some(dim), p.left, p.top, SRCCOPY);
                        let mut h = RECT::default();
                        if let Some(hole) = hole
                            && IntersectRect(&mut h, &hole, &p).as_bool()
                        {
                            let _ = BitBlt(hdc, h.left, h.top, h.right - h.left, h.bottom - h.top, Some(bright), h.left, h.top, SRCCOPY);
                        }
                    }
                    None => {
                        fill(hdc, p, rgb(0, 0, 0));
                        if let Some(h) = hole {
                            fill(hdc, h, KEY);
                        }
                    }
                }
                let _ = EndPaint(hwnd, &ps);
            }
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

unsafe extern "system" fn layer_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn toolbar_item_at(lp: LPARAM) -> Option<Item> {
    let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    with(|s| s.items.iter().find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom).map(|(i, _)| *i)).flatten()
}

/// A popup menu of `choices` under the toolbar item; returns the picked value.
fn pick(item: Item, choices: &[(u32, String, bool)]) -> Option<u32> {
    let (owner, at) = with(|s| {
        let r = s.items.iter().find(|(i, _)| *i == item)?.1;
        Some((s.overlays[s.active].hwnd, POINT { x: s.toolbar.pos.x + r.left, y: s.toolbar.pos.y + r.bottom + 4 }))
    })
    .flatten()?;
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, (_, label, checked)) in choices.iter().enumerate() {
            let t = wide(label);
            let flags = if *checked { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, flags, i + 1, PCWSTR(t.as_ptr()));
        }
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, at.x, at.y, None, owner, None);
        let _ = DestroyMenu(menu);
        (cmd.0 > 0).then(|| choices[cmd.0 as usize - 1].0)
    }
}

fn on_toolbar_click(item: Item) {
    match item {
        Item::Record => return record(),
        Item::Cancel => return finish(Outcome::Cancelled),
        Item::Kind(k) => return switch_kind(k),
        Item::Fps => {
            let Some((current, hz)) = with(|s| (s.settings.fps, s.overlays[s.active].display.refresh_hz)) else { return };
            let mut choices: Vec<(u32, String, bool)> =
                [30, 60, 120, 144].into_iter().filter(|&f| hz == 0 || f < hz).map(|f| (f, format!("{f} fps"), current == f)).collect();
            let max = if hz > 0 { format!("{hz} fps (refresh rate)") } else { "Refresh rate".into() };
            choices.push((0, max, current == 0));
            if let Some(f) = pick(item, &choices) {
                with(|s| s.settings.fps = f);
            }
        }
        Item::Delay => {
            let Some(current) = with(|s| delay_of(&s.settings, s.kind)) else { return };
            let choices: Vec<(u32, String, bool)> = [0, 3, 5, 10].into_iter().map(|d| (d, delay_label(d), current == d)).collect();
            if let Some(d) = pick(item, &choices) {
                with(|s| match s.kind {
                    Kind::Screenshot => s.settings.screenshot_delay_secs = d,
                    Kind::Video => s.settings.start_delay_secs = d,
                });
            }
        }
        _ => {
            with(|s| match item {
                Item::Mode(Mode::Region) => {
                    // A window or display picked in another mode isn't an area the user drew: start over.
                    if s.mode != Mode::Region {
                        s.sel = None;
                    }
                    s.mode = Mode::Region;
                    s.preview = None;
                }
                Item::Mode(Mode::Window) => {
                    s.mode = Mode::Window;
                    s.sel = None; // pick one by hovering
                }
                Item::Mode(Mode::Display) if s.kind == Kind::Screenshot => {
                    s.mode = Mode::Display; // pick a screen by clicking it
                    s.preview = Some(whole(&s.overlays[s.active].display));
                }
                Item::Mode(Mode::Display) => {
                    s.mode = Mode::Display;
                    s.preview = None;
                    s.sel = Some(full(&s.overlays[s.active].display));
                }
                Item::Audio => s.settings.capture_system_audio = !s.settings.capture_system_audio,
                Item::Cursor if s.kind == Kind::Screenshot => s.settings.screenshot_cursor = !s.settings.screenshot_cursor,
                Item::Cursor => s.settings.draw_mouse = !s.settings.draw_mouse,
                _ => {}
            });
        }
    }
    refresh();
}

unsafe extern "system" fn toolbar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let item = toolbar_item_at(lp);
            if with(|s| std::mem::replace(&mut s.hover, item) != item) == Some(true) {
                unsafe {
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut tme);
                }
                refresh();
            }
        }
        WM_MOUSELEAVE => {
            if with(|s| s.hover.take().is_some()) == Some(true) {
                refresh();
            }
        }
        WM_LBUTTONUP => {
            if let Some(item) = toolbar_item_at(lp).filter(|i| *i != Item::Mic) {
                on_toolbar_click(item);
            }
        }
        WM_SETCURSOR => unsafe {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let _ = ScreenToClient(hwnd, &mut p);
            let item = toolbar_item_at(LPARAM(((p.y as isize) << 16) | (p.x as isize & 0xFFFF)));
            let enabled = item.is_some_and(|i| i != Item::Mic && (i != Item::Record || with(|s| s.sel.is_some()) == Some(true)));
            SetCursor(LoadCursorW(None, if enabled { IDC_HAND } else { IDC_ARROW }).ok());
            return LRESULT(1);
        },
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: RECT = RECT { left: 100, top: 100, right: 500, bottom: 400 };

    #[test]
    fn hit_finds_corners_edges_inside_and_outside() {
        assert_eq!(hit(R, POINT { x: 102, y: 98 }, 8), Some((-1, -1)));
        assert_eq!(hit(R, POINT { x: 300, y: 405 }, 8), Some((0, 1)));
        assert_eq!(hit(R, POINT { x: 300, y: 250 }, 8), Some((0, 0)));
        assert_eq!(hit(R, POINT { x: 50, y: 250 }, 8), None);
    }

    #[test]
    fn resize_and_move_stay_on_screen_and_above_minimum() {
        let d = Display { output_index: 0, left: 0, top: 0, width: 1920, height: 1080, refresh_hz: 60 };
        let r = drag_rect(Drag::Resize { sx: 1, sy: 0, orig: R }, POINT { x: 120, y: 0 }, &d);
        assert_eq!((r.left, r.right - r.left), (100, MIN_W));
        let m = drag_rect(Drag::Move { grab: POINT { x: 300, y: 250 }, orig: R }, POINT { x: 5000, y: -5000 }, &d);
        assert_eq!((m.right, m.top), (1920, 0));
    }
}
