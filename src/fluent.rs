//! Windows 11 (WinUI 3 "Fluent") look for egui: Segoe UI + Segoe Fluent Icons, the dark-theme palette,
//! the system accent color, Mica behind the window, and the few controls egui lacks (toggle switch,
//! radio cards, accent button).

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Layout, Margin, Response,
    RichText, Sense, Stroke, TextStyle, Ui, Vec2, vec2,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
pub const TEXT: Color32 = Color32::WHITE;
pub const TEXT2: Color32 = Color32::from_gray(0xC5);
pub const TEXT3: Color32 = Color32::from_gray(0x9E);
/// Text and glyphs on an accent fill.
pub const ON_ACCENT: Color32 = Color32::BLACK;

// Translucent layers, as in WinUI's dark theme, so they also work over Mica.
pub const CARD: Color32 = Color32::from_rgba_premultiplied(13, 13, 13, 13);
pub const CARD_HOVER: Color32 = Color32::from_rgba_premultiplied(21, 21, 21, 21);
pub const CARD_STROKE: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 26);
pub const CONTROL: Color32 = Color32::from_rgba_premultiplied(15, 15, 15, 15);
pub const CONTROL_HOVER: Color32 = Color32::from_rgba_premultiplied(21, 21, 21, 21);
pub const CONTROL_PRESSED: Color32 = Color32::from_rgba_premultiplied(8, 8, 8, 8);
pub const CONTROL_STROKE: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);

/// Segoe Fluent Icons code points (same in Segoe MDL2 Assets on Windows 10).
pub mod icon {
    pub const PLAY: &str = "\u{E768}";
    pub const PAUSE: &str = "\u{E769}";
    pub const PREVIOUS: &str = "\u{E892}";
    pub const NEXT: &str = "\u{E893}";
    pub const CROP: &str = "\u{E7A8}";
    pub const COPY: &str = "\u{E8C8}";
    pub const FOLDER: &str = "\u{ED25}";
    pub const CHEVRON_DOWN: &str = "\u{E70D}";
    pub const IMAGE: &str = "\u{EB9F}";
    pub const ZOOM_IN: &str = "\u{E8A3}";
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

/// Fluent dark accent (the system's "AccentLight2", what WinUI uses on dark backgrounds).
pub fn system_accent() -> Color32 {
    use windows::UI::ViewManagement::{UIColorType, UISettings};
    UISettings::new()
        .and_then(|s| s.GetColorValue(UIColorType::AccentLight2))
        .map(|c| Color32::from_rgb(c.R, c.G, c.B))
        .unwrap_or(Color32::from_rgb(0x60, 0xCD, 0xFF))
}

/// Installs fonts and the dark Fluent style. Returns the accent color.
pub fn install(ctx: &egui::Context, mica: bool) -> Color32 {
    install_fonts(ctx);
    let accent = system_accent();
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        use TextStyle::*;
        style.text_styles = [
            (Small, FontId::proportional(12.0)),
            (Body, FontId::proportional(14.0)),
            (Button, FontId::proportional(14.0)),
            (Heading, semibold(20.0)),
            (Monospace, FontId::monospace(13.0)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = vec2(8.0, 8.0);
        s.button_padding = vec2(11.0, 5.0);
        s.interact_size = vec2(32.0, 32.0);
        s.combo_width = 172.0;
        s.icon_width = 18.0;
        s.icon_width_inner = 8.0;
        s.icon_spacing = 8.0;
        s.menu_margin = Margin::same(4);

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = if mica { Color32::TRANSPARENT } else { BG };
        v.window_fill = Color32::from_rgb(0x2C, 0x2C, 0x2C);
        v.window_stroke = Stroke::new(1.0, Color32::from_rgba_premultiplied(0, 0, 0, 51));
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = CONTROL;
        v.text_edit_bg_color = Some(Color32::from_rgba_premultiplied(0, 0, 0, 60));
        v.faint_bg_color = CARD;
        v.hyperlink_color = accent;
        v.weak_text_color = Some(TEXT2);
        v.selection.bg_fill = accent.gamma_multiply(0.3);
        v.selection.stroke = Stroke::new(1.0, accent);

        let w = &mut v.widgets;
        for (wv, fill) in [
            (&mut w.noninteractive, Color32::TRANSPARENT),
            (&mut w.inactive, CONTROL),
            (&mut w.hovered, CONTROL_HOVER),
            (&mut w.active, CONTROL_PRESSED),
            (&mut w.open, CONTROL_PRESSED),
        ] {
            wv.bg_fill = fill;
            wv.weak_bg_fill = fill;
            wv.bg_stroke = Stroke::new(1.0, CONTROL_STROKE);
            wv.fg_stroke = Stroke::new(1.0, TEXT);
            wv.corner_radius = CornerRadius::same(4);
            wv.expansion = 0.0;
        }
        w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgba_premultiplied(21, 21, 21, 21));
        w.active.fg_stroke = Stroke::new(1.0, TEXT2);
    });
    accent
}

fn install_fonts(ctx: &egui::Context) {
    let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    let load = |names: &[&str]| names.iter().find_map(|n| std::fs::read(dir.join(n)).ok());
    let mut fonts = FontDefinitions::default();
    let mut add = |key: &str, data: Option<Vec<u8>>| {
        let ok = data.is_some();
        if let Some(d) = data {
            fonts.font_data.insert(key.into(), Arc::new(FontData::from_owned(d)));
        }
        ok
    };
    let regular = add("segoe", load(&["segoeui.ttf"]));
    let bold = add("segoe-sb", load(&["seguisb.ttf"]));
    let icons = add("icons", load(&["SegoeIcons.ttf", "segmdl2.ttf"]));

    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    if regular {
        prop.insert(0, "segoe".into());
    }
    if icons {
        prop.push("icons".into()); // fallback, so icon code points work in any text
    }
    let mut sb: Vec<String> = Vec::new();
    if bold {
        sb.push("segoe-sb".into());
    }
    sb.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name("semibold".into()), sb);
    ctx.set_fonts(fonts);
}

/// Dark title bar and Mica. Returns whether Mica is on (Windows 11 22H2+), i.e. whether the client area should
/// be left transparent. A borderless `flyout` only gets rounded corners: extending the frame into it, which a
/// backdrop needs, would make DWM draw caption buttons on it.
pub fn style_window(hwnd: isize, flyout: bool) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::UI::Controls::MARGINS;
    let hwnd = HWND(hwnd as _);
    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        if flyout {
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
            return false;
        }
        let backdrop = DWMSBT_MAINWINDOW;
        let mica = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop as *const _ as _, 4).is_ok();
        if mica {
            let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
        } else {
            // Windows 10: no Mica; at least match the caption to the window (ignored where unsupported).
            let color: u32 = 0x0020_2020;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &color as *const _ as _, 4);
        }
        mica
    }
}

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
}

/// A plain button with an optional Fluent icon before the text.
pub fn button(ui: &mut Ui, icon: Option<&str>, text: &str) -> Response {
    let label = match icon {
        Some(i) => format!("{i}  {text}"),
        None => text.to_string(),
    };
    ui.add(egui::Button::new(label).min_size(vec2(0.0, 32.0)))
}

/// Filled accent button (WinUI "AccentButtonStyle").
pub fn accent_button(ui: &mut Ui, accent: Color32, text: &str, size: Vec2, enabled: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled {
        Color32::from_rgba_premultiplied(40, 40, 40, 40)
    } else if resp.is_pointer_button_down_on() {
        accent.gamma_multiply(0.8)
    } else if resp.hovered() {
        accent.gamma_multiply(0.9)
    } else {
        accent
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), fill);
    let color = if enabled { ON_ACCENT } else { TEXT3 };
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, semibold(14.0), color);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// WinUI standard button at a fixed size, label centered (egui's own button left-aligns once widened).
/// Follows `ui.is_enabled()`.
pub fn secondary_button(ui: &mut Ui, text: &str, size: Vec2) -> Response {
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let fill = if !enabled {
        CONTROL_PRESSED
    } else if resp.is_pointer_button_down_on() {
        CONTROL_PRESSED
    } else if resp.hovered() {
        CONTROL_HOVER
    } else {
        CONTROL
    };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(4), fill, Stroke::new(1.0, CONTROL_STROKE), egui::StrokeKind::Inside);
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, FontId::proportional(14.0), if enabled { TEXT } else { TEXT3 });
    resp
}

/// Two equal buttons side by side, filling the row exactly. `add` gets each one's size.
pub fn button_pair<R>(ui: &mut Ui, height: f32, add: impl FnOnce(&mut Ui, Vec2) -> R) -> R {
    let gap = 8.0;
    let size = vec2(((ui.available_width() - gap) / 2.0).floor(), height);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        add(ui, size)
    })
    .inner
}

/// Round transport button (play/pause).
pub fn round_button(ui: &mut Ui, glyph: &str, diameter: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        CONTROL_PRESSED
    } else if resp.hovered() {
        CONTROL_HOVER
    } else {
        CONTROL
    };
    let p = ui.painter();
    p.circle(rect.center(), diameter / 2.0 - 0.5, fill, Stroke::new(1.0, CONTROL_STROKE));
    p.text(rect.center(), egui::Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), TEXT);
    resp
}

/// Borderless icon button (subtle fill on hover).
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::click());
    if resp.hovered() {
        let fill = if resp.is_pointer_button_down_on() { CONTROL_PRESSED } else { CONTROL_HOVER };
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
    }
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), TEXT);
    resp.on_hover_text(tooltip)
}

/// Small toggle chip: accent outline and text when selected.
pub fn chip(ui: &mut Ui, accent: Color32, text: &str, selected: bool) -> Response {
    let rt = if selected { RichText::new(text).color(accent) } else { RichText::new(text) };
    let stroke = if selected { Stroke::new(1.0, accent) } else { Stroke::new(1.0, CONTROL_STROKE) };
    ui.add(egui::Button::new(rt).stroke(stroke).min_size(vec2(40.0, 32.0)))
}

/// Segmented selector (Windows Community Toolkit `Segmented`): equal segments, the selected one raised with an
/// accent bar. Returns the index clicked.
pub fn segmented(ui: &mut Ui, accent: Color32, labels: &[&str], selected: usize) -> Option<usize> {
    let (rect, area) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(6), CONTROL, Stroke::new(1.0, CONTROL_STROKE), egui::StrokeKind::Inside);
    // Equal segments, unless a label (bold when selected) doesn't fit one: then each gets room for its text.
    let need: Vec<f32> =
        labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), semibold(14.0), TEXT).size().x + 20.0).collect();
    let equal = rect.width() / labels.len() as f32;
    let widths: Vec<f32> = if need.iter().all(|n| *n <= equal) {
        vec![equal; labels.len()]
    } else {
        let total: f32 = need.iter().sum();
        need.iter().map(|n| n * rect.width() / total).collect()
    };
    let mut clicked = None;
    let mut left = rect.left();
    for (i, label) in labels.iter().enumerate() {
        let w = widths[i];
        let seg = egui::Rect::from_min_size(egui::pos2(left, rect.top()), vec2(w, rect.height())).shrink(3.0);
        left += w;
        // Ids derived from this control's own (auto) id: several segmented controls in one Ui mustn't share them.
        let resp = ui.interact(seg, area.id.with(i), Sense::click());
        let p = ui.painter();
        if i == selected {
            p.rect_filled(seg, CornerRadius::same(4), CONTROL_HOVER);
            let bar = egui::Rect::from_center_size(egui::pos2(seg.center().x, seg.bottom() - 1.5), vec2(16.0, 3.0));
            p.rect_filled(bar, CornerRadius::same(2), accent);
        } else if resp.hovered() {
            p.rect_filled(seg, CornerRadius::same(4), CONTROL_PRESSED);
        }
        let color = if i == selected { TEXT } else { TEXT2 };
        let font = if i == selected { semibold(14.0) } else { FontId::proportional(14.0) };
        p.text(seg.center() - vec2(0.0, 1.0), egui::Align2::CENTER_CENTER, *label, font, color);
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// WinUI ComboBox: fixed width, chevron glyph.
pub fn combo(id: &str, selected: impl Into<egui::WidgetText>) -> egui::ComboBox<'static> {
    egui::ComboBox::from_id_salt(id).width(200.0).selected_text(selected).icon(|ui, rect, visuals, _open| {
        let at = egui::pos2(rect.right() - 4.0, rect.center().y);
        let color = visuals.fg_stroke.color;
        ui.painter().text(at, egui::Align2::RIGHT_CENTER, icon::CHEVRON_DOWN, FontId::proportional(12.0), color);
    })
}

/// WinUI ToggleSwitch. In a right-to-left row (settings card) its On/Off label lands left of the switch,
/// as in Windows Settings.
pub fn toggle(ui: &mut Ui, accent: Color32, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 20.0), Sense::click());
    ui.label(RichText::new(if *on { "On" } else { "Off" }).small().color(TEXT2));
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let enabled = ui.is_enabled();
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let r = CornerRadius::same(10);
    if *on && enabled {
        p.rect_filled(rect, r, if resp.hovered() { accent.gamma_multiply(0.9) } else { accent });
    } else {
        p.rect(rect, r, CONTROL, Stroke::new(1.0, if enabled { TEXT2 } else { TEXT3 }), egui::StrokeKind::Inside);
    }
    let knob = if resp.hovered() && enabled { 7.0 } else { 6.0 };
    let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    let knob_color = if *on && enabled { ON_ACCENT } else if enabled { TEXT2 } else { TEXT3 };
    p.circle_filled(egui::pos2(x, rect.center().y), knob, knob_color);
    resp
}

/// A radio button drawn as a settings card (title + description); `body` shows under it while selected.
/// Returns true when the card was clicked.
pub fn radio_card(ui: &mut Ui, accent: Color32, selected: bool, enabled: bool, title: &str, sub: &str, body: impl FnOnce(&mut Ui)) -> bool {
    let id = ui.make_persistent_id(("radio_card", title));
    let hovered_before = ui.ctx().data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let fill = if hovered_before && enabled && !selected { CARD_HOVER } else { CARD };
    let inner = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let header = ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    let c = r.center();
                    if selected {
                        ui.painter().circle_filled(c, 9.0, if enabled { accent } else { TEXT3 });
                        ui.painter().circle_filled(c, 4.0, Color32::from_gray(0x1A));
                    } else {
                        ui.painter().circle(c, 8.5, CONTROL, Stroke::new(1.0, if enabled { TEXT2 } else { TEXT3 }));
                    }
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(title);
                        ui.label(RichText::new(sub).small().color(if enabled { TEXT2 } else { TEXT3 }));
                    });
                })
                .response
            });
            // Registered before the body, so the body's own widgets stay on top for clicks.
            let rect = header.inner.rect.expand2(vec2(16.0, 12.0)).with_max_x(ui.max_rect().right() + 16.0);
            let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
            if selected && enabled {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(32.0);
                    ui.vertical(body);
                });
            }
            resp
        });
    let resp = inner.inner;
    ui.ctx().data_mut(|d| d.insert_temp(id, resp.hovered()));
    if resp.hovered() && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// A label + control row inside a card (WinUI SettingsCard-like), control right-aligned.
pub fn setting_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) -> Response {
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        ui.label(RichText::new(label).color(TEXT2));
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    })
    .response
}
