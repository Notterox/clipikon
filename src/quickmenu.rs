//! The quick capture-settings flyout shown while the record hotkey is held (egui, Fluent style). Runs as its
//! own process, `clipikon quick-menu`, which the daemon starts as soon as the hotkey goes down, cloaked
//! (invisible, unfocused), so that it can appear instantly once the press counts as a hold (and is killed if it
//! doesn't).
//!
//! stdin, one command per line: `show` (place on the cursor's monitor and show), `record` (as if Record was
//! clicked); EOF closes it. stdout: `record <settings JSON>` when the user chose to record. The choices made
//! here are for that one recording: it starts from the saved settings and never writes them.

use crate::displays;
use crate::fluent::{self, TEXT2, TEXT3};
use crate::settings::Settings;
use eframe::egui::{self, Color32, Key, Margin, RichText, vec2};
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HWND, POINT, RECT};

pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Clipikon")
            .with_inner_size([360.0, 356.0])
            .with_resizable(false)
            .with_decorations(false)
            // eframe shows the window after its first frame regardless of `with_visible(false)`, so it's
            // cloaked from creation instead; and it must not take focus from the game until it's revealed.
            .with_active(false)
            .with_taskbar(false)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native("Clipikon quick menu", options, Box::new(|cc| Ok(Box::new(QuickMenu::new(cc)))))
        .map_err(|e| e.to_string())
}

const FPS_PRESETS: [u32; 4] = [30, 60, 120, 144];
const DELAYS: [u32; 4] = [0, 3, 5, 10];

struct QuickMenu {
    settings: Settings,
    hotkey: String,
    accent: Color32,
    /// Refresh rate of the monitor under the cursor; the "max" frame rate choice.
    hz: u32,
    /// Became focused after being shown; losing focus after that closes the flyout, like a menu.
    focused_once: bool,
    hwnd: isize,
    /// Set by the stdin thread on `show`.
    show_requested: Arc<AtomicBool>,
    /// Set by the stdin thread on `record` (the hotkey was pressed again).
    record_requested: Arc<AtomicBool>,
    /// After `show`, frames to paint while still cloaked before revealing (0 = nothing pending).
    reveal_in: u8,
    /// Cursor position at `show`, which the flyout centers on.
    anchor: POINT,
}

impl QuickMenu {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        if hwnd != 0 {
            set_cloak(HWND(hwnd as _), true);
            fluent::style_window(hwnd, true);
        }
        let accent = fluent::install(&cc.egui_ctx, false);
        let ctx = cc.egui_ctx.clone();
        let (show_requested, record_requested): (Arc<AtomicBool>, Arc<AtomicBool>) = Default::default();
        let (show, record) = (show_requested.clone(), record_requested.clone());
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                // Some writers (e.g. .NET's) start the stream with a UTF-8 BOM.
                match line.trim().trim_start_matches('\u{feff}') {
                    // Handled on the UI thread (`ui`), where moving the window and its DPI change are synchronous.
                    "show" => show.store(true, Ordering::Relaxed),
                    // Also on the UI thread, which holds the current choices.
                    "record" => record.store(true, Ordering::Relaxed),
                    _ => {}
                }
                ctx.request_repaint();
            }
            close_window(hwnd); // the daemon went away
        });
        let settings = Settings::load();
        Self {
            hotkey: settings.hotkey.clone(),
            settings,
            accent,
            hz: displays::under_cursor().map(|d| d.refresh_hz).unwrap_or(0),
            focused_once: false,
            hwnd,
            show_requested,
            record_requested,
            reveal_in: 0,
            anchor: POINT::default(),
        }
    }

    /// Moves the (still cloaked) window to the cursor; it's revealed a couple of frames later by
    /// `reveal_when_painted`, once it has its final size and has painted there.
    fn begin_show(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let hwnd = HWND(self.hwnd as _);
        unsafe {
            let _ = GetCursorPos(&mut self.anchor);
            // Onto the cursor's monitor first: if its scaling differs, the window is resized for it here.
            let work = work_area(self.anchor);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), work.left, work.top, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
        place(hwnd, self.anchor);
        self.reveal_in = 2;
    }

    /// Counts down the cloaked frames after `show`; then places the flyout once more (its size is final by
    /// now) and reveals it.
    fn reveal_when_painted(&mut self, ctx: &egui::Context) {
        if self.reveal_in == 0 {
            return;
        }
        self.reveal_in -= 1;
        if self.reveal_in > 0 {
            ctx.request_repaint();
            return;
        }
        let hwnd = HWND(self.hwnd as _);
        place(hwnd, self.anchor);
        set_cloak(hwnd, false);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }
    }

    /// Hands the choices to the daemon for this one recording, and closes.
    fn record(&self, ctx: &egui::Context) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "record {}", serde_json::to_string(&self.settings).unwrap_or_default());
        let _ = out.flush();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Frame-rate choices: the presets below the refresh rate, then the refresh rate itself (setting 0).
    fn fps_choices(&self) -> Vec<(u32, String)> {
        let mut v: Vec<(u32, String)> = FPS_PRESETS.iter().filter(|&&f| self.hz == 0 || f < self.hz).map(|&f| (f, f.to_string())).collect();
        v.push((0, if self.hz > 0 { format!("{} · max", self.hz) } else { "Max".into() }));
        v
    }
}

fn work_area(p: POINT) -> RECT {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST), &mut mi);
    }
    mi.rcWork
}

/// Centers the window on `p`, shifted as needed to stay fully inside that monitor's work area.
fn place(hwnd: HWND, p: POINT) {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let work = work_area(p);
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
        let margin = (8 * windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) / 96) as i32;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let fit = |c: i32, size: i32, lo: i32, hi: i32| (c - size / 2).clamp(lo + margin, (hi - margin - size).max(lo + margin));
        let (x, y) = (fit(p.x, w, work.left, work.right), fit(p.y, h, work.top, work.bottom));
        if (x, y) != (r.left, r.top) {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
}

fn set_cloak(hwnd: HWND, cloak: bool) {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    let value: i32 = cloak as i32;
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &value as *const _ as _, 4);
    }
}

fn close_window(hwnd: isize) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

impl eframe::App for QuickMenu {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.show_requested.swap(false, Ordering::Relaxed) {
            self.begin_show();
        }
        let (esc, enter, focused) = ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter), i.viewport().focused));
        if focused == Some(true) {
            self.focused_once = true;
        }
        if esc || (self.focused_once && focused == Some(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if enter || self.record_requested.swap(false, Ordering::Relaxed) {
            return self.record(&ctx);
        }

        let accent = self.accent;
        let frame = egui::Frame::new()
            // WinUI's flyout background (the solid fallback of its Acrylic).
            .fill(Color32::from_rgb(0x2C, 0x2C, 0x2C))
            .inner_margin(Margin::same(16));
        egui::CentralPanel::no_frame().frame(frame).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Quick settings").font(fluent::semibold(14.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&self.hotkey).small().color(TEXT3));
                });
            });
            ui.add_space(4.0);
            let width = ui.available_width();
            if fluent::accent_button(ui, accent, "Record region", vec2(width, 36.0), true).on_hover_text("Enter").clicked() {
                return self.record(&ctx);
            }
            ui.add_space(8.0);

            ui.label(RichText::new("Frame rate").color(TEXT2));
            let choices = self.fps_choices();
            let labels: Vec<&str> = choices.iter().map(|(_, l)| l.as_str()).collect();
            let current = choices.iter().position(|(f, _)| *f == self.settings.fps);
            // A saved rate that isn't offered here (e.g. 144 on a 60 Hz monitor) shows as no selection.
            if let Some(i) = fluent::segmented(ui, accent, &labels, current.unwrap_or(usize::MAX)) {
                self.settings.fps = choices[i].0;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Start delay").color(TEXT2));
            let delay_labels = ["Off", "3 s", "5 s", "10 s"];
            let current = DELAYS.iter().position(|d| *d == self.settings.start_delay_secs).unwrap_or(usize::MAX);
            if let Some(i) = fluent::segmented(ui, accent, &delay_labels, current) {
                self.settings.start_delay_secs = DELAYS[i];
            }
            ui.add_space(4.0);
            let s = &mut self.settings;
            fluent::setting_row(ui, "System audio", |ui| {
                fluent::toggle(ui, accent, &mut s.capture_system_audio);
            });
            fluent::setting_row(ui, "Show mouse cursor", |ui| {
                fluent::toggle(ui, accent, &mut s.draw_mouse);
            });
            ui.add_space(2.0);
            ui.label(RichText::new("This recording only · Enter records · Esc closes").small().color(TEXT3));
        });
        self.reveal_when_painted(&ctx);
    }
}
