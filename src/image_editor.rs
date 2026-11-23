//! Screenshot editor (egui, Fluent style): crop and mark up an image, then copy or save it. Runs as its own
//! process: `clipikon image <file.png>`.
//!
//! Marks are kept as shapes in image pixels, drawn over the image with egui's painter. Copy and Save flatten
//! them into the pixels with the same shapes: egui tessellates them at image scale and `raster` fills the
//! triangles, so the result matches what's on screen.

use crate::fluent::{self, TEXT, TEXT2, TEXT3};
use crate::settings::{EditorPrefs, Smoothing};
use serde::{Deserialize, Serialize};
use eframe::egui::epaint::{ClippedShape, Mesh, Primitive};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, pos2, vec2,
};
use std::path::{Path, PathBuf};

pub fn run(file: PathBuf) -> Result<(), String> {
    let data = std::fs::read(&file).map_err(|e| format!("Couldn't open {}: {e}", file.display()))?;
    let img = crate::screenshot::decode_png(&data).map_err(|e| format!("Couldn't read {}: {e}", file.display()))?;
    let size = [
        (img.width as f32 * 0.8 + 64.0).clamp(760.0, 1600.0),
        (img.height as f32 * 0.8 + 140.0).clamp(520.0, 1000.0),
    ];
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title(&file))
            .with_inner_size(size)
            .with_min_inner_size([640.0, 420.0])
            .with_icon(egui::IconData { rgba: crate::icon_art::render(64, crate::icon_art::Style::Color), width: 64, height: 64 })
            .with_transparent(true), // for Mica
        ..Default::default()
    };
    eframe::run_native("Clipikon", options, Box::new(move |cc| Ok(Box::new(ImageEditor::new(cc, file, img)))))
        .map_err(|e| e.to_string())
}

fn title(file: &Path) -> String {
    format!("Clipikon — {}", file.file_name().unwrap_or_default().to_string_lossy())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Select,
    Crop,
    Pen,
    Highlighter,
    Arrow,
    Rect,
    Text,
    Pixelate,
    Eraser,
    /// Eyedropper: takes a color from the image for the drawing tool in use.
    Picker,
}

const TOOLS: [Tool; 10] = [
    Tool::Select,
    Tool::Crop,
    Tool::Pen,
    Tool::Highlighter,
    Tool::Arrow,
    Tool::Rect,
    Tool::Text,
    Tool::Pixelate,
    Tool::Eraser,
    Tool::Picker,
];

impl Tool {
    fn index(self) -> usize {
        TOOLS.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn glyph(self) -> &'static str {
        match self {
            Tool::Select => "\u{E8B0}",
            Tool::Crop => "\u{E7A8}",
            Tool::Pen => "\u{E70F}",
            Tool::Highlighter => "\u{E7E6}",
            Tool::Arrow => "\u{E72A}",
            Tool::Rect => "\u{E739}",
            Tool::Text => "\u{E8D2}",
            Tool::Pixelate => "\u{ECA5}",
            Tool::Eraser => "\u{E75C}",
            Tool::Picker => "\u{EF3C}",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Tool::Select => "Select: drag a mark to move it, Delete removes it, double-click text to edit it",
            Tool::Crop => "Crop",
            Tool::Pen => "Pen",
            Tool::Highlighter => "Highlighter",
            Tool::Arrow => "Arrow",
            Tool::Rect => "Rectangle",
            Tool::Text => "Text (click a text to edit it)",
            Tool::Pixelate => "Pixelate, to hide private details",
            Tool::Eraser => "Eraser (removes whole marks)",
            Tool::Picker => "Color picker: click copies a color's hex code, Shift+click draws in it",
        }
    }

    fn has_color(self) -> bool {
        matches!(self, Tool::Pen | Tool::Highlighter | Tool::Arrow | Tool::Rect | Tool::Text)
    }

    fn has_size(self) -> bool {
        self.has_color() || self == Tool::Pixelate
    }

    /// Small, medium, large: line width, or text height, in image pixels.
    fn sizes(self) -> [f32; 3] {
        match self {
            Tool::Highlighter => [12.0, 20.0, 32.0],
            Tool::Text => [16.0, 24.0, 40.0],
            Tool::Pixelate => [8.0, 14.0, 24.0], // block size
            _ => [3.0, 5.0, 9.0],
        }
    }
}

const COLORS: [Color32; 7] = [
    Color32::from_rgb(0xE5, 0x39, 0x35),
    Color32::from_rgb(0xFB, 0x8C, 0x00),
    Color32::from_rgb(0xFD, 0xD8, 0x35),
    Color32::from_rgb(0x43, 0xA0, 0x47),
    Color32::from_rgb(0x1E, 0x88, 0xE5),
    Color32::WHITE,
    Color32::BLACK,
];
/// The highlighter's see-through.
const HIGHLIGHT_ALPHA: u8 = 110;
/// A tinted rectangle's fill.
const TINT_ALPHA: u8 = 64;

/// How a rectangle is filled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Fill {
    #[default]
    None,
    /// The outline color, see-through.
    Tint,
    Solid,
}

/// A mark on the image, in image pixels.
#[derive(Clone, PartialEq, Debug)]
enum Mark {
    /// Pen and highlighter (with a translucent color).
    Line { points: Vec<Pos2>, color: Color32, width: f32 },
    Arrow { from: Pos2, to: Pos2, color: Color32, width: f32 },
    Rect { rect: Rect, color: Color32, width: f32, fill: Fill },
    /// `pos` is the top-left corner; `size` the font size.
    Text { pos: Pos2, text: String, color: Color32, size: f32 },
    /// The image under `rect` in `block`-pixel squares. It's of the image itself, under every other mark.
    Pixelate { rect: Rect, block: f32 },
}

/// What undo and redo step through.
#[derive(Clone, PartialEq)]
struct Doc {
    marks: Vec<Mark>,
    /// The part of the image kept, in image pixels.
    crop: Rect,
}

/// The tool settings kept between sessions (in editor.json): the tool, each tool's color and size, the fill,
/// and the output size and quality.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ToolMemory {
    tool: String,
    /// (tool, #RRGGBB, size index)
    styles: Vec<(String, String, usize)>,
    rect_fill: String,
    output: Output,
}

/// How copies and saves come out.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Output {
    /// Of the cropped image, from MIN_SCALE to MAX_SCALE: below 1 shrinks, above 1 upscales.
    scale: f32,
    /// How an upscale fills in the new pixels.
    upscale: Upscale,
    /// JPEG rather than PNG: what Save as starts with, and what the preview shows.
    jpeg: bool,
    /// PNG reduced to 256 colors (lossy, usually much smaller).
    reduce_colors: bool,
    jpeg_quality: u8,
}

impl Default for Output {
    fn default() -> Self {
        Self { scale: 1.0, upscale: Upscale::Sharp, jpeg: false, reduce_colors: false, jpeg_quality: 90 }
    }
}

const MIN_SCALE: f32 = 0.1;
const MAX_SCALE: f32 = 4.0;
/// Upscales stop at about this many output pixels: beyond, the work takes gigabytes and seconds.
const MAX_OUTPUT_PIXELS: f32 = 40e6;

/// The largest scale for a `crop`-sized image.
fn max_scale(crop: Rect) -> f32 {
    (MAX_OUTPUT_PIXELS / (crop.width() * crop.height()).max(1.0)).sqrt().clamp(1.0, MAX_SCALE)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Upscale {
    /// Lanczos resampling, then a light unsharp mask: smooth edges, crisp detail.
    #[default]
    Sharp,
    /// Every pixel a block (nearest neighbor): exact, for pixel art and tiny UI.
    Pixels,
}

/// The output as previewed on the canvas while the Size & quality popup is open, worked out off the UI
/// thread: the job it's from, its pixels, and the PNG and JPEG sizes.
struct PreviewResult {
    job: u64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    png: Result<usize, String>,
    jpeg: Result<usize, String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    None,
    /// The selected tool's options.
    Tool,
    /// The color button's.
    Color,
    /// Size & quality of the output.
    Output,
}

#[derive(Clone, Copy)]
enum ZoomCmd {
    Fit,
    /// One image pixel per screen pixel.
    Actual,
    /// Multiply the zoom, about the middle of the view.
    By(f32),
}

#[derive(Clone, Copy)]
enum CropDrag {
    /// Drawing a new crop from this corner.
    New(Pos2),
    Move { grab: Pos2, orig: Rect },
    /// Edges being dragged: -1 = left/top, 1 = right/bottom, 0 = not on this axis.
    Resize { sx: i8, sy: i8, orig: Rect },
}

struct TextEdit {
    pos: Pos2,
    text: String,
    color: Color32,
    size: f32,
    focused: bool,
    /// The text mark being edited (hidden meanwhile), or None for a new one.
    replaces: Option<usize>,
}

/// Maps image pixels to the screen and back.
#[derive(Clone, Copy)]
struct View {
    /// The image part shown.
    src: Rect,
    /// Where it's shown.
    rect: Rect,
    /// Screen points per image pixel.
    scale: f32,
}

impl View {
    fn to_screen(&self, p: Pos2) -> Pos2 {
        self.rect.min + (p - self.src.min) * self.scale
    }
    fn to_image(&self, q: Pos2) -> Pos2 {
        self.src.min + (q - self.rect.min) / self.scale
    }
}

struct ImageEditor {
    file: PathBuf,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    texture: Option<egui::TextureHandle>,
    doc: Doc,
    undo: Vec<Doc>,
    redo: Vec<Doc>,
    /// The document as last saved, to know about unsaved changes.
    saved: Doc,
    tool: Tool,
    /// The tool to go back to after cropping.
    last_tool: Tool,
    /// How the next rectangle is filled.
    rect_fill: Fill,
    /// How much new pen and highlighter strokes are smoothed (saved in the editor prefs).
    smoothing: Smoothing,
    /// Which popup is open (the selected mark's shows by itself).
    popup: Popup,
    /// The color button, to put its popup under it.
    color_rect: Rect,
    /// A color change from the color popup has already saved an undo step (one per popup visit).
    recolored: bool,
    /// The color popup's hue, saturation and value (0..1), and its hex field's text.
    picker_hsv: [f32; 3],
    hex_text: String,
    /// Points per image pixel, or None to fit the window; and how far the image is moved from the middle.
    zoom: Option<f32>,
    pan: Vec2,
    /// The tool settings as last saved, to save them again when they change.
    remembered: ToolMemory,
    output: Output,
    /// The Size & quality button, to put its popup under it.
    output_rect: Rect,
    /// The preview job: what the latest was started for, its number, whether one is running, when the
    /// settings started differing from the last one, and where its result lands.
    preview_for: Option<(Output, Doc)>,
    preview_job: u64,
    preview_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    preview_pending: Option<std::time::Instant>,
    preview_result: std::sync::Arc<std::sync::Mutex<Option<PreviewResult>>>,
    /// The latest preview: its image and size, and the file sizes as text.
    preview: Option<(egui::TextureHandle, u32, u32)>,
    sizes_text: String,
    /// The scale used in the last frame (for the status bar), and a zoom asked for from keys or buttons.
    shown_scale: f32,
    zoom_cmd: Option<ZoomCmd>,
    /// Where each tool's button is, to put its popup under it.
    tool_rects: [Rect; TOOLS.len()],
    /// The selected mark on screen (Select tool), to put its options popup by it.
    sel_anchor: Option<Rect>,
    /// Per tool: its color (opaque) and index into its sizes.
    styles: [(Color32, usize); TOOLS.len()],
    /// The drawing tool last used: the color button and the color picker work on it when the current tool
    /// doesn't draw.
    last_draw: Tool,
    drawing: Option<Mark>,
    /// The crop being edited (Crop tool), and its drag.
    crop_edit: Option<(Rect, Option<CropDrag>)>,
    text: Option<TextEdit>,
    /// An eraser gesture has already saved an undo step.
    erasing: bool,
    /// The mark the Select tool picked, and where its drag was last.
    selected: Option<usize>,
    drag_last: Option<Pos2>,
    /// Pixelated blocks by area, so they aren't averaged again every frame.
    pixel_cache: std::cell::RefCell<std::collections::HashMap<[usize; 5], std::rc::Rc<Blocks>>>,
    accent: Color32,
    panel_fill: Color32,
    status: String,
    allow_close: bool,
}

impl ImageEditor {
    fn new(cc: &eframe::CreationContext<'_>, file: PathBuf, img: crate::screenshot::Image) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let mica = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => fluent::style_window(h.hwnd.get(), false),
            _ => false,
        };
        let accent = fluent::install(&cc.egui_ctx, mica);
        let rgba: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        let doc = Doc { marks: Vec::new(), crop: Rect::from_min_size(Pos2::ZERO, vec2(img.width as f32, img.height as f32)) };
        let mut styles = [(COLORS[0], 1); TOOLS.len()];
        styles[Tool::Highlighter.index()] = (COLORS[2], 1); // yellow
        // Ctrl +/-/0 zoom the image here, not the whole UI.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let mut editor = Self {
            file,
            width: img.width,
            height: img.height,
            rgba,
            texture: None,
            saved: doc.clone(),
            doc,
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Pen,
            last_tool: Tool::Pen,
            styles,
            last_draw: Tool::Pen,
            drawing: None,
            crop_edit: None,
            text: None,
            erasing: false,
            selected: None,
            rect_fill: Fill::None,
            smoothing: EditorPrefs::load().smoothing,
            popup: Popup::None,
            color_rect: Rect::NOTHING,
            recolored: false,
            picker_hsv: [0.0, 0.0, 1.0],
            hex_text: String::new(),
            zoom: None,
            pan: Vec2::ZERO,
            shown_scale: 1.0,
            remembered: ToolMemory::default(),
            output: Output::default(),
            output_rect: Rect::NOTHING,
            preview_for: None,
            preview_job: 0,
            preview_running: Default::default(),
            preview_pending: None,
            preview_result: Default::default(),
            preview: None,
            sizes_text: String::new(),
            zoom_cmd: None,
            tool_rects: [Rect::NOTHING; TOOLS.len()],
            sel_anchor: None,
            drag_last: None,
            pixel_cache: Default::default(),
            accent,
            panel_fill: if mica { Color32::TRANSPARENT } else { fluent::BG },
            status: String::new(),
            allow_close: false,
        };
        if let Some(m) = EditorPrefs::load().image_tools.and_then(|v| serde_json::from_value(v).ok()) {
            editor.restore(&m);
        }
        editor.remembered = editor.memory();
        editor
    }

    /// The tool settings worth keeping for next time.
    fn memory(&self) -> ToolMemory {
        // Crop and the color picker are passing steps: come back to the drawing tool.
        let tool = if matches!(self.tool, Tool::Crop | Tool::Picker) { self.last_draw } else { self.tool };
        ToolMemory {
            tool: format!("{tool:?}"),
            styles: TOOLS.iter().filter(|t| t.has_size()).map(|t| (format!("{t:?}"), hex(self.styles[t.index()].0), self.styles[t.index()].1)).collect(),
            rect_fill: format!("{:?}", self.rect_fill),
            output: self.output,
        }
    }

    fn restore(&mut self, m: &ToolMemory) {
        let tool = |name: &str| TOOLS.into_iter().find(|t| format!("{t:?}") == name);
        for (name, color, size) in &m.styles {
            if let (Some(t), Some(c)) = (tool(name), parse_hex(color)) {
                self.styles[t.index()] = (c, (*size).min(2));
            }
        }
        if let Some(t) = tool(&m.tool).filter(|t| !matches!(t, Tool::Crop | Tool::Picker)) {
            self.tool = t;
            if t.has_color() {
                self.last_draw = t;
            }
        }
        self.rect_fill = [Fill::None, Fill::Tint, Fill::Solid].into_iter().find(|f| format!("{f:?}") == m.rect_fill).unwrap_or_default();
        self.output = Output {
            scale: if m.output.scale.is_finite() { m.output.scale.clamp(MIN_SCALE, MAX_SCALE) } else { 1.0 },
            upscale: m.output.upscale,
            jpeg: m.output.jpeg,
            reduce_colors: m.output.reduce_colors,
            jpeg_quality: m.output.jpeg_quality.clamp(40, 100),
        };
    }

    /// Saves the tool settings when they've changed (not mid-drag, so a color being dragged isn't written
    /// every frame).
    fn remember(&mut self, ctx: &egui::Context) {
        let m = self.memory();
        if m != self.remembered && !ctx.input(|i| i.pointer.any_down()) {
            let mut prefs = EditorPrefs::load();
            prefs.image_tools = serde_json::to_value(&m).ok();
            prefs.save();
            self.remembered = m;
        }
    }

    fn full(&self) -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(self.width as f32, self.height as f32))
    }

    fn color(&self, tool: Tool) -> Color32 {
        let c = self.styles[tool.index()].0;
        if tool == Tool::Highlighter { Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), HIGHLIGHT_ALPHA) } else { c }
    }

    /// The drawing tool whose color the color button and the picker change.
    fn draw_tool(&self) -> Tool {
        if self.tool.has_color() { self.tool } else { self.last_draw }
    }

    /// The selected mark, if it has a color (Select tool).
    fn selected_colored(&self) -> Option<usize> {
        self.selected.filter(|i| self.tool == Tool::Select && self.doc.marks.get(*i).is_some_and(|m| mark_tool(m).has_color()))
    }

    /// The color the color button shows: the selected mark's, else the drawing tool's. Opaque.
    fn target_color(&self) -> Color32 {
        match self.selected_colored().and_then(|i| self.doc.marks.get(i)) {
            Some(m) => opaque(mark_color(m).unwrap_or(Color32::WHITE)),
            None => self.styles[self.draw_tool().index()].0,
        }
    }

    /// Sets the color button's color. On a mark, one undo step per visit to the popup (the picker drags).
    fn set_target_color(&mut self, c: Color32) {
        if let Some(i) = self.selected_colored() {
            if !self.recolored {
                self.undo.push(self.doc.clone());
                self.redo.clear();
                self.recolored = true;
            }
            restyle(&mut self.doc.marks[i], Some(c), None);
        } else {
            let t = self.draw_tool();
            self.styles[t.index()].0 = c;
            let color = self.color(t);
            if let Some(text) = &mut self.text {
                text.color = color;
            }
        }
    }

    fn size(&self, tool: Tool) -> f32 {
        tool.sizes()[self.styles[tool.index()].1]
    }

    /// Records an undo step and changes the document.
    fn change(&mut self, f: impl FnOnce(&mut Doc)) {
        self.undo.push(self.doc.clone());
        self.redo.clear();
        f(&mut self.doc);
    }

    fn undo(&mut self) {
        self.finish_text();
        self.selected = None;
        if let Some(d) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.doc, d));
            self.crop_edit = None;
        }
    }

    fn redo(&mut self) {
        self.finish_text();
        self.selected = None;
        if let Some(d) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.doc, d));
            self.crop_edit = None;
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        self.finish_text();
        self.selected = None;
        if self.tool.has_color() {
            self.last_draw = self.tool;
        }
        if tool.has_color() {
            self.last_draw = tool;
        }
        if (tool == Tool::Crop) != (self.tool == Tool::Crop) {
            self.zoom_cmd = Some(ZoomCmd::Fit); // cropping shows the whole image
        }
        if tool == Tool::Crop {
            if self.tool != Tool::Crop {
                self.last_tool = self.tool;
            }
            self.crop_edit = Some((self.doc.crop, None));
        } else {
            self.crop_edit = None;
        }
        self.tool = tool;
    }

    fn apply_crop(&mut self) {
        if let Some((r, _)) = self.crop_edit.take() {
            let r = round_rect(r).intersect(self.full());
            if r != self.doc.crop && r.width() >= 1.0 && r.height() >= 1.0 {
                self.change(|d| d.crop = r);
            }
        }
        self.tool = self.last_tool;
        self.zoom_cmd = Some(ZoomCmd::Fit);
    }

    fn cancel_crop(&mut self) {
        self.crop_edit = None;
        self.tool = self.last_tool;
        self.zoom_cmd = Some(ZoomCmd::Fit);
    }

    /// Commits the text being typed, if any.
    fn finish_text(&mut self) {
        let Some(t) = self.text.take() else { return };
        let empty = t.text.trim().is_empty();
        let mark = Mark::Text { pos: t.pos, text: t.text, color: t.color, size: t.size };
        match t.replaces {
            // An edit: an emptied text goes away; an unchanged one adds no undo step.
            Some(i) if i < self.doc.marks.len() => {
                if self.doc.marks[i] != mark {
                    self.change(|d| {
                        if empty {
                            d.marks.remove(i);
                        } else {
                            d.marks[i] = mark;
                        }
                    });
                }
            }
            _ if !empty => self.change(|d| d.marks.push(mark)),
            _ => {}
        }
    }

    /// Opens text mark `i` for typing, with the Text tool.
    fn edit_text(&mut self, i: usize) {
        let Some(Mark::Text { pos, text, color, size }) = self.doc.marks.get(i).cloned() else { return };
        self.finish_text();
        self.selected = None;
        self.tool = Tool::Text;
        let (_, z) = style_of(&self.doc.marks[i], Tool::Text);
        self.styles[Tool::Text.index()] = (opaque(color), z);
        self.text = Some(TextEdit { pos, text, color, size, focused: false, replaces: Some(i) });
    }

    /// The topmost mark at `at` (image pixels).
    fn hit_mark(&self, at: Pos2, scale: f32, ctx: &egui::Context) -> Option<usize> {
        let painter = egui::Painter::new(ctx.clone(), egui::LayerId::background(), Rect::EVERYTHING);
        let slack = 6.0 / scale.max(0.01);
        self.doc.marks.iter().rposition(|m| hits(m, at, slack, &painter))
    }

    /// Pixelated blocks of `rect`, cached.
    fn blocks(&self, rect: Rect, block: f32) -> Option<std::rc::Rc<Blocks>> {
        let r = round_rect(rect).intersect(self.full());
        let key = [r.min.x as usize, r.min.y as usize, r.max.x as usize, r.max.y as usize, block.round() as usize];
        let mut cache = self.pixel_cache.borrow_mut();
        if let Some(b) = cache.get(&key) {
            return Some(b.clone());
        }
        let b = std::rc::Rc::new(Blocks::new(&self.rgba, self.width, rect, block)?);
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(key, b.clone());
        Some(b)
    }

    /// Pixelation drawn through `view`: one colored quad per block.
    fn pixel_shape(&self, rect: Rect, block: f32, view: &View) -> Shape {
        let mut mesh = Mesh::default();
        if let Some(b) = self.blocks(rect, block) {
            for (i, c) in b.colors.iter().enumerate() {
                let (sx, sy) = (b.x0 + (i % b.cols) * b.size, b.y0 + (i / b.cols) * b.size);
                let (ex, ey) = ((sx + b.size).min(b.x1), (sy + b.size).min(b.y1));
                let r = Rect::from_min_max(view.to_screen(pos2(sx as f32, sy as f32)), view.to_screen(pos2(ex as f32, ey as f32)));
                mesh.add_colored_rect(r, Color32::from_rgb(c[0], c[1], c[2]));
            }
        }
        Shape::mesh(mesh)
    }

    /// The options popup's content: color, size and the tool's extras, for the selected mark (Select tool) or
    /// else for the tool's next mark.
    fn style_panel(&mut self, ui: &mut egui::Ui) {
        let sel = self.selected.filter(|_| self.tool == Tool::Select).and_then(|i| Some((i, self.doc.marks.get(i)?.clone())));
        let kind = sel.as_ref().map_or(self.tool, |(_, m)| mark_tool(m));
        let (color, size) = match &sel {
            Some((_, m)) => style_of(m, kind),
            None => (palette_index(self.styles[kind.index()].0), self.styles[kind.index()].1),
        };
        // A color not in the palette (from the picker or the color button) shows as an extra swatch.
        let custom = match &sel {
            Some((_, m)) => mark_color(m).map(opaque),
            None => Some(self.styles[kind.index()].0),
        }
        .filter(|_| color.is_none());
        let section = |ui: &mut egui::Ui, text: &str| {
            ui.label(egui::RichText::new(text).size(12.0).color(TEXT3));
        };
        ui.set_width(7.0 * 28.0 + 6.0 * 4.0);
        ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
        let (mut new_color, mut new_size) = (None, None);
        if kind.has_color() {
            section(ui, "Color");
            ui.horizontal(|ui| {
                for (k, c) in COLORS.iter().enumerate() {
                    if swatch(ui, *c, color == Some(k)).clicked() {
                        new_color = Some(*c);
                    }
                }
            });
            if let Some(c) = custom {
                ui.horizontal(|ui| {
                    swatch(ui, c, true);
                    ui.label(egui::RichText::new(hex(c)).color(TEXT2));
                });
            }
        }
        if kind.has_size() {
            section(ui, if kind == Tool::Pixelate { "Block size" } else if kind == Tool::Text { "Text size" } else { "Thickness" });
            ui.horizontal(|ui| {
                for (k, label) in ["Small", "Medium", "Large"].iter().enumerate() {
                    if size_button(ui, k, size == k).on_hover_text(*label).clicked() {
                        new_size = Some(k);
                    }
                }
            });
        }
        // Smoothing is for new strokes, so it's offered with the tool, not for a selected stroke.
        if sel.is_none() && matches!(kind, Tool::Pen | Tool::Highlighter) {
            section(ui, "Smoothing");
            let labels: Vec<&str> = Smoothing::ALL.iter().map(|l| l.label()).collect();
            let current = Smoothing::ALL.iter().position(|l| *l == self.smoothing).unwrap_or(2);
            if let Some(k) = fluent::segmented(ui, self.accent, &labels, current) {
                self.smoothing = Smoothing::ALL[k];
                let mut prefs = EditorPrefs::load();
                prefs.smoothing = self.smoothing;
                prefs.save();
            }
        }
        if kind == Tool::Rect {
            section(ui, "Fill");
            let current = match &sel {
                Some((_, Mark::Rect { fill, .. })) => *fill,
                _ => self.rect_fill,
            };
            ui.horizontal(|ui| {
                for (f, tip) in [(Fill::None, "Outline"), (Fill::Tint, "Outline with a light fill"), (Fill::Solid, "Filled")] {
                    if fill_button(ui, f, current == f).on_hover_text(tip).clicked() && f != current {
                        match &sel {
                            Some((i, Mark::Rect { .. })) => {
                                let i = *i;
                                self.change(|d| {
                                    if let Mark::Rect { fill, .. } = &mut d.marks[i] {
                                        *fill = f;
                                    }
                                });
                            }
                            _ => self.rect_fill = f,
                        }
                    }
                }
            });
        }
        if new_color.is_none() && new_size.is_none() {
            return;
        }
        match sel {
            Some((i, mut m)) => {
                restyle(&mut m, new_color, new_size.map(|k| kind.sizes()[k]));
                if m != self.doc.marks[i] {
                    self.change(|d| d.marks[i] = m);
                }
            }
            None => {
                let i = kind.index();
                if let Some(c) = new_color {
                    self.styles[i].0 = c;
                }
                if let Some(k) = new_size {
                    self.styles[i].1 = k;
                }
                let (color, size) = (self.color(kind), self.size(kind));
                if let Some(t) = &mut self.text {
                    t.color = color;
                    t.size = size;
                }
            }
        }
    }

    /// The open popup: the tool's options under its button, the color button's, or else the selected mark's
    /// options by it.
    fn popups(&mut self, ctx: &egui::Context) {
        if self.popup != Popup::Color {
            self.recolored = false;
        }
        if self.popup == Popup::Tool && (!self.tool.has_size() || self.crop_edit.is_some()) {
            self.popup = Popup::None;
        }
        let anchor = match self.popup {
            Popup::Tool => Some(self.tool_rects[self.tool.index()]),
            Popup::Color => Some(self.color_rect),
            Popup::Output => Some(self.output_rect),
            Popup::None if self.tool == Tool::Select && self.text.is_none() => self.sel_anchor,
            Popup::None => None,
        };
        let Some(anchor) = anchor else { return };
        let kind = self.popup;
        let id = egui::Id::unique(match kind {
            Popup::Tool => "tool-options",
            Popup::Color => "color-options",
            Popup::Output => "output-options",
            Popup::None => "mark-options",
        });
        let resp = egui::Area::new(id)
            .fixed_pos(pos2(anchor.left(), anchor.bottom() + 6.0))
            .order(egui::Order::Foreground)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Color32::from_rgb(0x2C, 0x2C, 0x2C))
                    .stroke(Stroke::new(1.0, Color32::from_white_alpha(22)))
                    .corner_radius(8)
                    .inner_margin(12)
                    .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(100) })
                    .show(ui, |ui| {
                        match kind {
                            Popup::Color => self.color_panel(ui),
                            Popup::Output => self.output_panel(ui),
                            _ => self.style_panel(ui),
                        }
                    });
            })
            .response;
        // A popup from a button closes on Esc, or a press outside it and that button (which toggles it).
        if kind != Popup::None {
            let outside = ctx.input(|i| {
                i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !resp.rect.contains(p) && !anchor.contains(p))
            });
            if outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.popup = Popup::None;
            }
        }
    }

    /// The Size & quality popup: output size (and how to upscale), PNG colors, JPEG quality, which format the
    /// canvas previews, and the resulting file sizes.
    fn output_panel(&mut self, ui: &mut egui::Ui) {
        ui.set_width(240.0);
        ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
        ui.spacing_mut().slider_width = 170.0;
        let section = |ui: &mut egui::Ui, text: &str| {
            ui.label(egui::RichText::new(text).size(12.0).color(TEXT3));
        };
        let crop = round_rect(self.doc.crop);

        section(ui, "Size");
        let mut pct = (self.output.scale * 100.0).round();
        ui.add(
            egui::Slider::new(&mut pct, MIN_SCALE * 100.0..=(max_scale(crop) * 100.0).floor())
                .logarithmic(true)
                .suffix("%")
                .step_by(1.0)
                .custom_formatter(|v, _| format!("{v:.0}")),
        );
        self.output.scale = (pct / 100.0).clamp(MIN_SCALE, max_scale(crop));
        let (w, h) = ((crop.width() * self.output.scale).round().max(1.0), (crop.height() * self.output.scale).round().max(1.0));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{w} × {h} pixels")).color(TEXT2));
            if self.output.scale != 1.0 && ui.small_button("100%").clicked() {
                self.output.scale = 1.0;
            }
        });
        if self.output.scale > 1.0 {
            section(ui, "Upscale");
            let k = usize::from(self.output.upscale == Upscale::Pixels);
            if let Some(k) = fluent::segmented(ui, self.accent, &["Sharp", "Pixels"], k) {
                self.output.upscale = if k == 1 { Upscale::Pixels } else { Upscale::Sharp };
            }
        }

        section(ui, "Format");
        if let Some(k) = fluent::segmented(ui, self.accent, &["PNG", "JPEG"], usize::from(self.output.jpeg)) {
            self.output.jpeg = k == 1;
        }
        if self.output.jpeg {
            section(ui, "Quality");
            let mut q = self.output.jpeg_quality;
            ui.add(egui::Slider::new(&mut q, 40..=100));
            self.output.jpeg_quality = q;
        } else {
            section(ui, "Colors");
            let k = usize::from(self.output.reduce_colors);
            if let Some(k) = fluent::segmented(ui, self.accent, &["Lossless", "256 colors"], k) {
                self.output.reduce_colors = k == 1;
            }
        }
        let text = if self.sizes_text.is_empty() { "File size: working it out…".to_string() } else { format!("File size: {}", self.sizes_text) };
        ui.label(egui::RichText::new(text).color(TEXT2));
        ui.label(egui::RichText::new("Save as starts with this format; Copy is always PNG.").size(12.0).color(TEXT3));
        self.start_preview(ui.ctx());
    }

    /// Starts working out the preview when the settings or the image changed: right away once the pointer is
    /// up, every 300 ms while a slider is dragged, and never two at once.
    fn start_preview(&mut self, ctx: &egui::Context) {
        use std::sync::atomic::Ordering;
        let key = (self.output, self.doc.clone());
        if self.preview_for.as_ref() == Some(&key) {
            self.preview_pending = None;
            return;
        }
        let since = *self.preview_pending.get_or_insert_with(std::time::Instant::now);
        let settled = !ctx.input(|i| i.pointer.any_down()) || since.elapsed() > std::time::Duration::from_millis(300);
        if !settled || self.preview_running.load(Ordering::Acquire) {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }
        self.preview_for = Some(key);
        self.preview_pending = None;
        self.preview_job += 1;
        let job = self.preview_job;
        let (w, h, rgba) = self.render(ctx);
        let (opts, jpeg_view) = (self.output, self.output.jpeg);
        let (slot, running, ctx) = (self.preview_result.clone(), self.preview_running.clone(), ctx.clone());
        running.store(true, Ordering::Release);
        std::thread::spawn(move || {
            let (w, h, px) = resize_image(w, h, rgba, opts.scale.min(max_scale(Rect::from_min_size(Pos2::ZERO, vec2(w as f32, h as f32)))), opts.upscale);
            let png_px = if opts.reduce_colors { reduce_colors(&px) } else { px.clone() };
            let png = crate::screenshot::encode_png(w, h, &png_px, crate::screenshot::Order::Rgba);
            let jpeg = encode_output(w, h, &px, true, opts);
            let shown = match (&jpeg, jpeg_view) {
                (Ok(data), true) => decode_jpeg(data).unwrap_or(png_px),
                _ => png_px,
            };
            *slot.lock().unwrap() =
                Some(PreviewResult { job, width: w, height: h, rgba: shown, png: png.map(|d| d.len()), jpeg: jpeg.map(|d| d.len()) });
            running.store(false, Ordering::Release);
            ctx.request_repaint();
        });
    }

    /// Takes a finished preview: its image for the canvas, its sizes for the popup.
    fn take_preview(&mut self, ctx: &egui::Context) {
        let Some(r) = self.preview_result.lock().unwrap().take() else { return };
        if r.job != self.preview_job {
            return; // superseded
        }
        let size = |s: &Result<usize, String>| s.as_ref().map(|n| kb(*n)).unwrap_or_else(|e| e.clone());
        self.sizes_text = size(if self.output.jpeg { &r.jpeg } else { &r.png });
        let img = ColorImage::from_rgba_unmultiplied([r.width as usize, r.height as usize], &r.rgba);
        let mut opts = egui::TextureOptions::LINEAR;
        opts.mipmap_mode = Some(egui::TextureFilter::Linear);
        opts.magnification = egui::TextureFilter::Nearest;
        let tex = ctx.load_texture("output-preview", img, opts);
        self.preview = Some((tex, r.width, r.height));
    }

    /// The color button's popup: the palette, a full color picker, and the hex code.
    fn color_panel(&mut self, ui: &mut egui::Ui) {
        let before = self.target_color();
        let mut c = before;
        ui.set_width(7.0 * 28.0 + 6.0 * 4.0);
        ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
        let what = match self.selected_colored().and_then(|i| self.doc.marks.get(i)) {
            Some(m) => format!("Color of the selected {}", mark_tool(m).noun()),
            None => format!("{} color", self.draw_tool().noun_capitalized()),
        };
        ui.label(egui::RichText::new(what).size(12.0).color(TEXT3));
        ui.horizontal(|ui| {
            for p in COLORS {
                if swatch(ui, p, palette_index(c) == palette_index(p)).clicked() {
                    c = p;
                }
            }
        });
        // Keep the hue and saturation while they can't be read back from the color (grey, black).
        if hsv_to_rgb(self.picker_hsv) != c {
            self.picker_hsv = rgb_to_hsv(c, self.picker_hsv);
        }
        if color_square(ui, &mut self.picker_hsv) | hue_bar(ui, &mut self.picker_hsv) {
            c = hsv_to_rgb(self.picker_hsv);
        }
        // Swatch and hex field centered on one row, the field out to the popup's edge like the bars above.
        let row = egui::Layout::left_to_right(egui::Align::Center);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), row, |ui| {
            if swatch(ui, c, false).on_hover_text("Copy the hex code").clicked() {
                ui.ctx().copy_text(hex(c));
                self.status = format!("Copied {}.", hex(c));
            }
            let edit = egui::TextEdit::singleline(&mut self.hex_text)
                .desired_width(ui.available_width())
                .font(FontId::monospace(13.0))
                .align(Align2::LEFT_CENTER);
            let r = ui.add(edit).on_hover_text("Hex color, like #1E88E5");
            if r.has_focus() {
                if let Some(typed) = parse_hex(&self.hex_text) {
                    c = typed;
                }
            } else {
                self.hex_text = hex(c);
            }
        });
        if c != before {
            self.set_target_color(c);
        }
    }

    fn dirty(&self) -> bool {
        self.doc != self.saved
    }

    // ---- output ----

    /// The image as it looks: cropped, with the marks drawn in. RGBA.
    fn render(&self, ctx: &egui::Context) -> (u32, u32, Vec<u8>) {
        let crop = round_rect(self.doc.crop);
        let (x0, y0) = (crop.min.x as usize, crop.min.y as usize);
        let (w, h) = (crop.width() as usize, crop.height() as usize);
        let mut buf = Vec::with_capacity(w * h * 4);
        for y in y0..y0 + h {
            let row = (y * self.width as usize + x0) * 4;
            buf.extend_from_slice(&self.rgba[row..row + w * 4]);
        }
        // Tessellate at image scale: egui positions are points, so 1 image pixel = 1/ppp points.
        let ppp = ctx.pixels_per_point();
        let painter = egui::Painter::new(ctx.clone(), egui::LayerId::background(), Rect::EVERYTHING);
        let view = View { src: crop, rect: Rect::from_min_size(Pos2::ZERO, vec2(w as f32, h as f32) / ppp), scale: 1.0 / ppp };
        for m in &self.doc.marks {
            if let Mark::Pixelate { rect, block } = m
                && let Some(b) = self.blocks(*rect, *block)
            {
                for y in b.y0.max(y0)..b.y1.min(y0 + h) {
                    for x in b.x0.max(x0)..b.x1.min(x0 + w) {
                        let c = b.colors[((y - b.y0) / b.size) * b.cols + (x - b.x0) / b.size];
                        let d = ((y - y0) * w + (x - x0)) * 4;
                        buf[d..d + 3].copy_from_slice(&c);
                    }
                }
            }
        }
        let mut shapes = Vec::new();
        for m in &self.doc.marks {
            mark_shapes(m, &view, &painter, &mut shapes);
        }
        if shapes.is_empty() {
            return (w as u32, h as u32, buf);
        }
        let clipped = shapes.into_iter().map(|shape| ClippedShape { clip_rect: Rect::EVERYTHING, shape, transform_after_tessellation: egui::emath::TSTransform::IDENTITY }).collect();
        let primitives = ctx.tessellate(clipped, ppp);
        let atlas = ctx.fonts(|f| f.image());
        for p in primitives {
            if let Primitive::Mesh(mesh) = p.primitive {
                raster(&mut buf, w, h, &mesh, ppp, &atlas);
            }
        }
        (w as u32, h as u32, buf)
    }

    /// The image as copied and saved: rendered, then shrunk to the output size.
    fn output_image(&self, ctx: &egui::Context) -> (u32, u32, Vec<u8>) {
        let (w, h, rgba) = self.render(ctx);
        resize_image(w, h, rgba, self.output.scale.min(max_scale(self.doc.crop)), self.output.upscale)
    }

    fn copy(&mut self, ctx: &egui::Context) {
        self.finish_text();
        let (w, h, mut rgba) = self.output_image(ctx);
        if self.output.reduce_colors {
            rgba = reduce_colors(&rgba); // the bitmap matches the PNG
        }
        self.status = match crate::clipboard::copy_image(w, h, &rgba) {
            Ok(()) => format!("Copied ({w}×{h})."),
            Err(e) => format!("Couldn't copy: {e}"),
        };
    }

    /// Writes the image to `path`: JPEG for .jpg/.jpeg, else a compact PNG.
    fn save_to(&mut self, ctx: &egui::Context, path: &Path) -> Result<(), String> {
        self.finish_text();
        let (w, h, rgba) = self.output_image(ctx);
        let jpeg = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
        let data = encode_output(w, h, &rgba, jpeg, self.output)?;
        std::fs::write(path, data).map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
        self.saved = self.doc.clone();
        Ok(())
    }

    fn save(&mut self, ctx: &egui::Context) -> bool {
        let file = self.file.clone();
        match self.save_to(ctx, &file) {
            Ok(()) => {
                self.status = format!("Saved {}.", file.file_name().unwrap_or_default().to_string_lossy());
                true
            }
            Err(e) => {
                self.status = e;
                false
            }
        }
    }

    fn save_as(&mut self, ctx: &egui::Context) {
        let stem = self.file.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let (png, jpeg) = (("PNG", &["png"][..]), ("JPEG", &["jpg", "jpeg"][..]));
        let (first, second, ext) = if self.output.jpeg { (jpeg, png, "jpg") } else { (png, jpeg, "png") };
        let mut dlg = rfd::FileDialog::new().add_filter(first.0, first.1).add_filter(second.0, second.1).set_file_name(format!("{stem}.{ext}"));
        if let Some(dir) = self.file.parent() {
            dlg = dlg.set_directory(dir);
        }
        let Some(mut path) = dlg.save_file() else { return };
        if path.extension().is_none() {
            path.set_extension(if self.output.jpeg { "jpg" } else { "png" });
        }
        match self.save_to(ctx, &path) {
            Ok(()) => {
                self.status = format!("Saved as {}.", path.file_name().unwrap_or_default().to_string_lossy());
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title(&path)));
                self.file = path;
            }
            Err(e) => self.status = e,
        }
    }

    /// Asks about unsaved changes when the window is closed.
    fn confirm_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_close {
            return;
        }
        self.finish_text();
        if !self.dirty() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        use windows::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDYES, MB_ICONWARNING, MB_YESNOCANCEL, MessageBoxW};
        let text = crate::win::wide(&format!(
            "Save changes to {}?",
            self.file.file_name().unwrap_or_default().to_string_lossy()
        ));
        let answer = unsafe { MessageBoxW(None, windows::core::PCWSTR(text.as_ptr()), windows::core::w!("Clipikon"), MB_YESNOCANCEL | MB_ICONWARNING) };
        let close = match answer {
            IDCANCEL => false,
            IDYES => self.save(ctx),
            _ => true,
        };
        if close {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.text.is_some() || ctx.egui_wants_keyboard_input() {
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace))
            && let Some(i) = self.selected.take()
            && i < self.doc.marks.len()
        {
            self.change(|d| {
                d.marks.remove(i);
            });
        }
        let (undo, redo, copy, save, save_as, enter, esc) = ctx.input_mut(|i| {
            use egui::{KeyboardShortcut, Modifiers};
            let cmd = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
            let cs = Modifiers::COMMAND | Modifiers::SHIFT;
            // The Shift ones first: a Ctrl shortcut also matches with Shift held.
            let redo = i.consume_shortcut(&cmd(cs, Key::Z)) || i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Y));
            let save_as = i.consume_shortcut(&cmd(cs, Key::S));
            if i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Num0)) {
                self.zoom_cmd = Some(ZoomCmd::Fit);
            } else if i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Num1)) {
                self.zoom_cmd = Some(ZoomCmd::Actual);
            } else if i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Equals)) || i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Plus)) {
                self.zoom_cmd = Some(ZoomCmd::By(1.25));
            } else if i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Minus)) {
                self.zoom_cmd = Some(ZoomCmd::By(0.8));
            }
            (
                i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::Z)),
                redo,
                // egui turns Ctrl+C into a Copy event rather than a key press.
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.consume_shortcut(&cmd(Modifiers::COMMAND, Key::S)),
                save_as,
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
            )
        });
        // Shift+Ctrl shortcuts first: Ctrl+S would otherwise also match Ctrl+Shift+S.
        if redo {
            self.redo();
        } else if undo {
            self.undo();
        }
        if copy {
            self.copy(ctx);
        }
        if save_as {
            self.save_as(ctx);
        } else if save {
            self.save(ctx);
        }
        if self.crop_edit.is_some() {
            if enter {
                self.apply_crop();
            } else if esc {
                self.cancel_crop();
            }
        }
    }

    // ---- toolbar ----

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Undo … Save as keep the right end; the tools and their options get the rest of the width and wrap
        // onto a second row when the window is too narrow, instead of running under those buttons.
        const ACTIONS_W: f32 = 6.0 * 36.0 + 9.0 + 7.0 * 4.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let left = (ui.available_width() - ACTIONS_W - 12.0).max(120.0);
            let wrap = egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true);
            ui.allocate_ui_with_layout(vec2(left, 36.0), wrap, |ui| self.tools_row(ui));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if plain_button(ui, "\u{E792}", "Save as… (Ctrl+Shift+S)", true).clicked() {
                    self.save_as(&ctx);
                }
                if plain_button(ui, "\u{E74E}", "Save (Ctrl+S)", true).clicked() {
                    self.save(&ctx);
                }
                if plain_button(ui, "\u{E8C8}", "Copy (Ctrl+C)", true).clicked() {
                    self.copy(&ctx);
                }
                let r = plain_button(ui, "\u{E713}", "Size & quality of copies and saves", true);
                if r.clicked() {
                    self.popup = if self.popup == Popup::Output { Popup::None } else { Popup::Output };
                }
                self.output_rect = r.rect;
                separator(ui);
                if plain_button(ui, "\u{E7A6}", "Redo (Ctrl+Y)", !self.redo.is_empty()).clicked() {
                    self.redo();
                }
                if plain_button(ui, "\u{E7A7}", "Undo (Ctrl+Z)", !self.undo.is_empty()).clicked() {
                    self.undo();
                }
            });
        });
    }

    /// The tools, and the crop buttons while cropping. A tool's options are in its popup.
    fn tools_row(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        for t in TOOLS {
            if t == Tool::Picker {
                // The color button, then the eyedropper beside it.
                separator(ui);
                let r = color_button(ui, self.target_color(), self.popup == Popup::Color).on_hover_text("Color");
                if r.clicked() {
                    self.popup = if self.popup == Popup::Color { Popup::None } else { Popup::Color };
                }
                self.color_rect = r.rect;
            }
            let tip = if t.has_size() { format!("{} — click again for options", t.name()) } else { t.name().to_string() };
            let r = tool_button(ui, t.glyph(), self.tool == t, t.has_size()).on_hover_text(tip);
            if r.clicked() {
                if self.tool == t && t.has_size() {
                    self.popup = if self.popup == Popup::Tool { Popup::None } else { Popup::Tool };
                } else {
                    self.set_tool(t);
                    self.popup = Popup::None;
                }
            }
            if r.secondary_clicked() && t.has_size() {
                self.set_tool(t);
                self.popup = Popup::Tool;
            }
            self.tool_rects[t.index()] = r.rect;
            if t == Tool::Crop {
                separator(ui);
            }
        }
        if self.crop_edit.is_some() {
            separator(ui);
            ui.add(egui::Label::new(egui::RichText::new("Drag the edges, or draw a new area").color(TEXT2)).extend());
            ui.add_space(8.0);
            if fluent::secondary_button(ui, "Reset", vec2(72.0, 32.0)).clicked() {
                let full = self.full();
                if let Some((r, _)) = &mut self.crop_edit {
                    *r = full;
                }
            }
            if fluent::secondary_button(ui, "Cancel", vec2(72.0, 32.0)).clicked() {
                self.cancel_crop();
            }
            if fluent::accent_button(ui, self.accent, "Apply", vec2(72.0, 32.0), true).clicked() {
                self.apply_crop();
            }
        }
    }

    // ---- canvas ----

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area = ui.available_rect_before_wrap();
        let texture = self
            .texture
            .get_or_insert_with(|| {
                let img = ColorImage::from_rgba_unmultiplied([self.width as usize, self.height as usize], &self.rgba);
                let mut opts = egui::TextureOptions::LINEAR;
                opts.mipmap_mode = Some(egui::TextureFilter::Linear); // smooth when shown smaller
                opts.magnification = egui::TextureFilter::Nearest; // sharp pixels when zoomed in
                ctx.load_texture("image", img, opts)
            })
            .id();
        let src = if self.crop_edit.is_some() { self.full() } else { self.doc.crop };
        let resp = ui.allocate_rect(area, Sense::click_and_drag());
        let view = self.place(&ctx, &resp, area, src);
        let (rect, scale) = (view.rect, view.scale);
        let painter = ui.painter_at(area);
        let size = vec2(self.width as f32, self.height as f32);
        let uv = Rect::from_min_max((src.min.to_vec2() / size).to_pos2(), (src.max.to_vec2() / size).to_pos2());
        painter.image(texture, rect, uv, Color32::WHITE);

        let clipped = painter.with_clip_rect(rect);
        let hidden = self.text.as_ref().and_then(|t| t.replaces);
        let preview = self.drawing.as_ref().map(|m| smoothed(m, self.smoothing)); // as it will be kept
        let marks: Vec<&Mark> =
            self.doc.marks.iter().enumerate().filter(|(i, _)| Some(*i) != hidden).map(|(_, m)| m).chain(preview.as_ref()).collect();
        let mut shapes = Vec::new();
        // Pixelation is of the image itself, so under every other mark (as in `render`).
        for m in &marks {
            if let Mark::Pixelate { rect, block } = m {
                shapes.push(self.pixel_shape(*rect, *block, &view));
            }
        }
        for m in &marks {
            mark_shapes(m, &view, &clipped, &mut shapes);
        }
        clipped.extend(shapes);
        // While Size & quality is open: the output as it will come out, over the image and its marks.
        if self.popup == Popup::Output
            && self.crop_edit.is_none()
            && let Some((tex, pw, ph)) = &self.preview
        {
            painter.rect_filled(rect, CornerRadius::ZERO, fluent::BG);
            painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            let kind = if self.output.jpeg {
                format!("JPEG {}", self.output.jpeg_quality)
            } else if self.output.reduce_colors {
                "PNG, 256 colors".into()
            } else {
                "PNG".into()
            };
            let g = painter.layout_no_wrap(format!("Preview: {kind}, {pw} × {ph}"), FontId::proportional(12.0), TEXT);
            let pill = Rect::from_min_size(pos2(area.center().x - g.size().x / 2.0 - 8.0, area.top() + 8.0), g.size() + vec2(16.0, 8.0));
            painter.rect_filled(pill, CornerRadius::same(4), Color32::from_rgba_unmultiplied(0x20, 0x20, 0x20, 220));
            painter.galley(pill.min + vec2(8.0, 4.0), g, TEXT);
        }
        self.sel_anchor = None;
        if self.tool == Tool::Select
            && let Some(m) = self.selected.and_then(|i| self.doc.marks.get(i))
        {
            let b = mark_bounds(m, &clipped);
            let r = Rect::from_two_pos(view.to_screen(b.min), view.to_screen(b.max)).expand(4.0);
            painter.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.5, self.accent), StrokeKind::Outside);
            // Hidden while it's being moved, so it doesn't jump around under the pointer.
            if self.drag_last.is_none() {
                self.sel_anchor = Some(r.intersect(rect).intersect(area));
            }
        }

        let panning = self.panning(&ctx);
        if panning {
            // Space held: a drag moves the image instead of drawing.
        } else if self.crop_edit.is_some() {
            self.crop_input(&resp, &view);
        } else {
            self.draw_input(&resp, &view, &ctx);
        }
        if self.crop_edit.is_some() {
            self.draw_crop(&painter, &view);
        }
        self.text_box(ui, &view);
        if self.tool == Tool::Picker
            && !panning
            && let Some(pos) = resp.hover_pos()
            && let Some(c) = self.pixel(view.to_image(pos))
        {
            loupe(&painter, pos, c);
        }
        let _ = scale;
        if panning && resp.hovered() {
            ctx.set_cursor_icon(if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        } else if let Some(pos) = resp.hover_pos()
            && rect.contains(pos)
        {
            let icon = match self.tool {
                Tool::Text => egui::CursorIcon::Text,
                Tool::Select if self.hit_mark(view.to_image(pos), view.scale, &ctx).is_some() => egui::CursorIcon::Move,
                Tool::Select | Tool::Crop => egui::CursorIcon::Default,
                _ => egui::CursorIcon::Crosshair,
            };
            if self.crop_edit.is_none() {
                ctx.set_cursor_icon(icon);
            }
        }
    }

    /// Space held (and not typing): drags pan instead of drawing.
    fn panning(&self, ctx: &egui::Context) -> bool {
        self.text.is_none() && !ctx.egui_wants_keyboard_input() && ctx.input(|i| i.key_down(Key::Space))
    }

    /// Where the image goes in `area` this frame: applies wheel zoom about the pointer, middle-button (or
    /// Space) drag panning, and the zoom keys and buttons.
    fn place(&mut self, ctx: &egui::Context, resp: &egui::Response, area: Rect, src: Rect) -> View {
        let ppp = ctx.pixels_per_point();
        // Fit: never larger than 1 image pixel per screen pixel.
        let fit = (area.width() / src.width()).min(area.height() / src.height()).min(1.0 / ppp);
        let (min, max) = (fit.min(0.05), 32.0 / ppp);
        let mut scale = self.zoom.unwrap_or(fit).clamp(min, max);
        let rect_for = |scale: f32, pan: Vec2| Rect::from_center_size(area.center() + pan, src.size() * scale);
        // Zooms to `to`, keeping the image point under `at` where it is.
        let zoom_about = |scale: &mut f32, pan: &mut Vec2, to: f32, at: Pos2| {
            let to = to.clamp(min, max);
            let before = rect_for(*scale, *pan);
            let q = (at - before.min) / *scale; // image offset from the shown part's corner
            let min_corner = at - q * to;
            *pan = (min_corner + src.size() * to / 2.0) - area.center();
            *scale = to;
        };
        let mut pan = self.pan;
        if let Some(cmd) = self.zoom_cmd.take() {
            match cmd {
                ZoomCmd::Fit => {
                    scale = fit;
                    pan = Vec2::ZERO;
                }
                ZoomCmd::Actual => zoom_about(&mut scale, &mut pan, 1.0 / ppp, area.center()),
                ZoomCmd::By(f) => {
                    let to = scale * f;
                    zoom_about(&mut scale, &mut pan, to, area.center())
                }
            }
        }
        if let Some(at) = resp.hover_pos() {
            let (zoom, scroll) = ctx.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            // The wheel zooms about the pointer (as Ctrl+wheel and pinch do); a sideways scroll (Shift+wheel)
            // pans. A notch of the wheel is about 50 points of scroll: about 1.3×.
            let factor = zoom * (scroll.y / 200.0).exp();
            if factor != 1.0 {
                let to = scale * factor;
                zoom_about(&mut scale, &mut pan, to, at);
            }
            pan.x += scroll.x;
        }
        if resp.dragged_by(egui::PointerButton::Middle) || (self.panning(ctx) && resp.dragged_by(egui::PointerButton::Primary)) {
            pan += resp.drag_delta();
        }
        // Keep the image in view: centered on an axis where it fits, edge to edge at most where it doesn't.
        let size = src.size() * scale;
        let slack = ((size - area.size()) / 2.0).max(Vec2::ZERO);
        pan = pan.clamp(-slack, slack);
        self.zoom = if (scale - fit).abs() < 1e-4 && pan == Vec2::ZERO { None } else { Some(scale) };
        self.pan = pan;
        self.shown_scale = scale;
        View { src, rect: rect_for(scale, pan), scale }
    }

    /// The image's color at `p` (image pixels), if it's on the image.
    fn pixel(&self, p: Pos2) -> Option<Color32> {
        let (x, y) = (p.x.floor(), p.y.floor());
        if x < 0.0 || y < 0.0 || x >= self.width as f32 || y >= self.height as f32 {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Some(Color32::from_rgb(self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]))
    }

    fn draw_input(&mut self, resp: &egui::Response, view: &View, ctx: &egui::Context) {
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let Some(at) = pointer.map(|p| view.to_image(p)) else { return };
        let (color, width) = (self.color(self.tool), self.size(self.tool));
        if resp.clicked() {
            match self.tool {
                Tool::Pen | Tool::Highlighter => {
                    self.finish_text();
                    self.change(|d| d.marks.push(Mark::Line { points: vec![at], color, width }));
                }
                Tool::Text => {
                    // A click while typing just finishes that text; one on a text edits it.
                    let on_text = self.hit_mark(at, view.scale, ctx).filter(|i| matches!(self.doc.marks[*i], Mark::Text { .. }));
                    if self.text.is_some() {
                        self.finish_text();
                    } else if let Some(i) = on_text {
                        self.edit_text(i);
                    } else {
                        let pos = at - vec2(0.0, width * 0.6); // the click lands mid-line
                        self.text = Some(TextEdit { pos, text: String::new(), color, size: width, focused: false, replaces: None });
                    }
                }
                Tool::Select => {
                    self.selected = self.hit_mark(at, view.scale, ctx);
                    if resp.double_clicked()
                        && let Some(i) = self.selected
                        && matches!(self.doc.marks[i], Mark::Text { .. })
                    {
                        self.edit_text(i);
                    }
                }
                Tool::Eraser => self.erase_at(at, view.scale, ctx, false),
                // A click copies the color's hex code (and stays, to pick more); Shift+click draws with it.
                Tool::Picker => {
                    if let Some(c) = self.pixel(at) {
                        if ctx.input(|i| i.modifiers.shift) {
                            let t = self.last_draw;
                            self.styles[t.index()].0 = c;
                            self.status = format!("Drawing in {} with the {}.", hex(c), t.noun());
                            self.set_tool(t);
                        } else {
                            ctx.copy_text(hex(c));
                            self.status = format!("Copied {}. Shift+click to draw in it.", hex(c));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        let primary = egui::PointerButton::Primary;
        if resp.drag_started_by(primary) {
            self.finish_text();
            let origin = ctx.input(|i| i.pointer.press_origin()).map(|p| view.to_image(p)).unwrap_or(at);
            self.drawing = match self.tool {
                Tool::Pen | Tool::Highlighter => Some(Mark::Line { points: vec![origin, at], color, width }),
                Tool::Arrow => Some(Mark::Arrow { from: origin, to: at, color, width }),
                Tool::Rect => Some(Mark::Rect { rect: Rect::from_two_pos(origin, at), color, width, fill: self.rect_fill }),
                Tool::Pixelate => Some(Mark::Pixelate { rect: Rect::from_two_pos(origin, at), block: width }),
                Tool::Select => {
                    // Dragging a mark moves it, as one undo step.
                    self.selected = self.hit_mark(origin, view.scale, ctx);
                    if self.selected.is_some() {
                        self.undo.push(self.doc.clone());
                        self.redo.clear();
                        self.drag_last = Some(origin);
                    }
                    None
                }
                Tool::Eraser => {
                    self.erasing = false;
                    self.erase_at(at, view.scale, ctx, true);
                    None
                }
                _ => None,
            };
        } else if resp.dragged_by(primary) {
            // About 1.5 screen points apart; the highlighter's wider, so its path stays smooth.
            let min_step = match &self.drawing {
                Some(Mark::Line { color, width, .. }) if color.a() < 255 => width * 0.35,
                _ => (1.5 / view.scale.max(0.01)).min(4.0),
            };
            match &mut self.drawing {
                Some(Mark::Line { points, .. }) if points.last().is_none_or(|p| p.distance(at) >= min_step) => points.push(at),
                Some(Mark::Arrow { to, .. }) => *to = at,
                Some(Mark::Rect { rect, .. }) | Some(Mark::Pixelate { rect, .. }) => *rect = Rect::from_two_pos(rect_anchor(*rect, at), at),
                _ if self.tool == Tool::Eraser => self.erase_at(at, view.scale, ctx, true),
                _ if self.tool == Tool::Select => {
                    if let (Some(i), Some(last)) = (self.selected, self.drag_last)
                        && let Some(m) = self.doc.marks.get_mut(i)
                    {
                        translate(m, at - last);
                        self.drag_last = Some(at);
                    }
                }
                _ => {}
            }
        }
        if resp.drag_stopped_by(primary) {
            self.erasing = false;
            self.drag_last = None;
            if let Some(m) = self.drawing.take()
                && !is_empty(&m)
            {
                let m = smoothed(&m, self.smoothing);
                self.change(|d| d.marks.push(m));
            }
        }
    }

    /// Removes the marks at `at`. A drag keeps one undo step for all it erases.
    fn erase_at(&mut self, at: Pos2, scale: f32, ctx: &egui::Context, gesture: bool) {
        let painter = egui::Painter::new(ctx.clone(), egui::LayerId::background(), Rect::EVERYTHING);
        let slack = 6.0 / scale.max(0.01);
        let hit: Vec<usize> = self.doc.marks.iter().enumerate().filter(|(_, m)| hits(m, at, slack, &painter)).map(|(i, _)| i).collect();
        if hit.is_empty() {
            return;
        }
        if !gesture || !self.erasing {
            self.undo.push(self.doc.clone());
            self.redo.clear();
            self.erasing = gesture;
        }
        for i in hit.into_iter().rev() {
            self.doc.marks.remove(i);
        }
        self.selected = None;
    }

    fn crop_input(&mut self, resp: &egui::Response, view: &View) {
        let Some((rect, drag)) = &mut self.crop_edit else { return };
        let full = Rect::from_min_size(Pos2::ZERO, view.src.size());
        let margin = 8.0 / view.scale;
        let cursor = |h: Option<(i8, i8)>| match h {
            Some((0, 0)) => egui::CursorIcon::Move,
            Some((-1, -1)) | Some((1, 1)) => egui::CursorIcon::ResizeNwSe,
            Some((1, -1)) | Some((-1, 1)) => egui::CursorIcon::ResizeNeSw,
            Some((_, 0)) => egui::CursorIcon::ResizeHorizontal,
            Some((0, _)) => egui::CursorIcon::ResizeVertical,
            _ => egui::CursorIcon::Crosshair,
        };
        if let Some(p) = resp.hover_pos() {
            let h = match drag {
                Some(CropDrag::Move { .. }) => Some((0, 0)),
                Some(CropDrag::Resize { sx, sy, .. }) => Some((*sx, *sy)),
                Some(CropDrag::New(_)) => None,
                None => crop_hit(*rect, view.to_image(p), margin),
            };
            resp.ctx.set_cursor_icon(cursor(h));
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            let origin = resp.ctx.input(|i| i.pointer.press_origin()).map(|p| view.to_image(p));
            // While the crop is the whole image there's nothing to move: a drag inside draws the area.
            let whole = *rect == full;
            *drag = origin.map(|o| match crop_hit(*rect, o, margin) {
                Some((0, 0)) if whole => CropDrag::New(clamp_pos(o, full)),
                Some((0, 0)) => CropDrag::Move { grab: o, orig: *rect },
                Some((sx, sy)) => CropDrag::Resize { sx, sy, orig: *rect },
                None => CropDrag::New(clamp_pos(o, full)),
            });
        }
        if let (Some(d), Some(p)) = (*drag, resp.interact_pointer_pos().filter(|_| resp.dragged_by(egui::PointerButton::Primary))) {
            let p = view.to_image(p);
            *rect = match d {
                CropDrag::New(a) => Rect::from_two_pos(a, clamp_pos(p, full)),
                CropDrag::Move { grab, orig } => {
                    let dx = (p.x - grab.x).clamp(-orig.min.x, full.max.x - orig.max.x);
                    let dy = (p.y - grab.y).clamp(-orig.min.y, full.max.y - orig.max.y);
                    orig.translate(vec2(dx, dy))
                }
                CropDrag::Resize { sx, sy, orig } => resize(orig, sx, sy, clamp_pos(p, full)),
            };
        }
        if resp.drag_stopped_by(egui::PointerButton::Primary) {
            *drag = None;
            if rect.width() < 4.0 || rect.height() < 4.0 {
                *rect = full; // a click or a tiny drag: start over from the whole image
            }
        }
    }

    fn draw_crop(&self, painter: &egui::Painter, view: &View) {
        let Some((r, _)) = self.crop_edit else { return };
        let s = Rect::from_two_pos(view.to_screen(r.min), view.to_screen(r.max));
        let dim = Color32::from_black_alpha(150);
        let o = view.rect;
        for part in [
            Rect::from_min_max(o.min, pos2(o.max.x, s.min.y)),
            Rect::from_min_max(pos2(o.min.x, s.max.y), o.max),
            Rect::from_min_max(pos2(o.min.x, s.min.y), pos2(s.min.x, s.max.y)),
            Rect::from_min_max(pos2(s.max.x, s.min.y), pos2(o.max.x, s.max.y)),
        ] {
            painter.rect_filled(part, CornerRadius::ZERO, dim);
        }
        painter.rect_stroke(s, CornerRadius::ZERO, Stroke::new(2.0, Color32::WHITE), StrokeKind::Outside);
        // Thirds, as a framing guide.
        let thin = Stroke::new(1.0, Color32::from_white_alpha(90));
        for k in [1.0, 2.0] {
            let x = s.min.x + s.width() * k / 3.0;
            let y = s.min.y + s.height() * k / 3.0;
            painter.line_segment([pos2(x, s.min.y), pos2(x, s.max.y)], thin);
            painter.line_segment([pos2(s.min.x, y), pos2(s.max.x, y)], thin);
        }
        let hs = 10.0;
        for (x, y) in [
            (s.min.x, s.min.y), (s.max.x, s.min.y), (s.min.x, s.max.y), (s.max.x, s.max.y),
            (s.center().x, s.min.y), (s.center().x, s.max.y), (s.min.x, s.center().y), (s.max.x, s.center().y),
        ] {
            let h = Rect::from_center_size(pos2(x, y), Vec2::splat(hs));
            painter.rect(h, CornerRadius::same(2), Color32::WHITE, Stroke::new(1.0, Color32::from_gray(0x20)), StrokeKind::Inside);
        }
        let label = format!("{} × {}", r.width().round(), r.height().round());
        let at = pos2(s.min.x, s.max.y + 8.0);
        let g = painter.layout_no_wrap(label, FontId::proportional(13.0), TEXT);
        let bg = Rect::from_min_size(at, g.size() + vec2(12.0, 6.0));
        painter.rect_filled(bg, CornerRadius::same(4), Color32::from_rgb(0x2B, 0x2B, 0x2B));
        painter.galley(at + vec2(6.0, 3.0), g, TEXT);
    }

    /// The text being typed, edited in place at its size and color.
    fn text_box(&mut self, ui: &mut egui::Ui, view: &View) {
        let Some(t) = &mut self.text else { return };
        let font = FontId::proportional(t.size * view.scale);
        let at = view.to_screen(t.pos);
        let measure = ui.painter().layout_no_wrap(if t.text.is_empty() { "M".into() } else { t.text.clone() }, font.clone(), t.color);
        let width = measure.size().x + font.size;
        let mut cancel = false;
        let resp = egui::Area::new(egui::Id::unique("image-text"))
            .fixed_pos(at)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                // An area offers only last frame's width, and a text field never grows past what it's offered:
                // widen it to the text each frame, so the box grows as you type. The font size of slack covers
                // the character typed this frame.
                ui.set_max_width(width);
                let edit = egui::TextEdit::singleline(&mut t.text)
                    .font(font)
                    .text_color(t.color)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .clip_text(false)
                    .desired_width(width);
                let r = ui.add(edit);
                ui.painter().rect_stroke(r.rect.expand(3.0), CornerRadius::same(2), Stroke::new(1.0, Color32::from_white_alpha(160)), StrokeKind::Outside);
                cancel = ui.input(|i| i.key_pressed(Key::Escape));
                r
            })
            .inner;
        if !t.focused {
            resp.request_focus();
            t.focused = true;
        } else if resp.lost_focus() {
            if cancel {
                self.text = None;
            } else {
                self.finish_text();
            }
        }
    }
}

impl eframe::App for ImageEditor {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // Mica shows through; without Mica the panels paint the background
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.confirm_close(&ctx);
        self.keyboard(&ctx);
        self.take_preview(&ctx);
        let fill = self.panel_fill;
        egui::Panel::top("toolbar")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 8 }))
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin { left: 16, right: 16, top: 2, bottom: 8 }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let crop = round_rect(self.doc.crop);
                    let mut size = format!("{} × {}", crop.width(), crop.height());
                    if self.output.scale < 1.0 {
                        let s = self.output.scale;
                        size += &format!(" → {} × {}", (crop.width() * s).round().max(1.0), (crop.height() * s).round().max(1.0));
                    }
                    ui.label(egui::RichText::new(size).color(TEXT3));
                    if !self.status.is_empty() {
                        ui.label(egui::RichText::new(&self.status).color(TEXT2));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let small = |ui: &mut egui::Ui, text: &str, tip: &str| {
                            ui.add(egui::Button::new(egui::RichText::new(text).size(13.0)).frame(false).min_size(vec2(28.0, 22.0))).on_hover_text(tip)
                        };
                        if small(ui, "Fit", "Fit the window (Ctrl+0)").clicked() {
                            self.zoom_cmd = Some(ZoomCmd::Fit);
                        }
                        if small(ui, "\u{E710}", "Zoom in (mouse wheel, Ctrl+=)").clicked() {
                            self.zoom_cmd = Some(ZoomCmd::By(1.25));
                        }
                        let pct = (self.shown_scale * ctx.pixels_per_point() * 100.0).round();
                        if small(ui, &format!("{pct}%"), "Actual size (Ctrl+1)").clicked() {
                            self.zoom_cmd = Some(ZoomCmd::Actual);
                        }
                        if small(ui, "\u{E738}", "Zoom out (mouse wheel, Ctrl+-)").clicked() {
                            self.zoom_cmd = Some(ZoomCmd::By(0.8));
                        }
                    });
                });
            });
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 4 }))
            .show(ui, |ui| self.canvas(ui));
        self.popups(&ctx);
        self.remember(&ctx);
    }
}

// ---- shapes ----

/// Adds the egui shapes that draw `m` through `view`.
fn mark_shapes(m: &Mark, view: &View, painter: &egui::Painter, out: &mut Vec<Shape>) {
    let s = view.scale;
    match m {
        Mark::Line { points, color, width } => {
            let w = width * s;
            let pts: Vec<Pos2> = points.iter().map(|p| view.to_screen(*p)).collect();
            let opaque = color.a() == 255;
            if pts.len() == 1 {
                out.push(Shape::circle_filled(pts[0], w / 2.0, *color));
                return;
            }
            if opaque {
                // Separate segments with round joints and ends: one path would get sharp (mitered) corners
                // that spike out where the stroke turns quickly.
                for seg in pts.windows(2) {
                    out.push(Shape::line_segment([seg[0], seg[1]], Stroke::new(w, *color)));
                }
                for p in &pts {
                    out.push(Shape::circle_filled(*p, w / 2.0, *color));
                }
            } else {
                // Translucent: one path, so overlaps don't darken. Its points are spaced out (see draw_input),
                // which keeps the corners gentle.
                out.push(Shape::line(pts, Stroke::new(w, *color)));
            }
        }
        Mark::Arrow { from, to, color, width } => {
            let (a, b) = (view.to_screen(*from), view.to_screen(*to));
            let w = width * s;
            let len = a.distance(b);
            if len < 0.5 {
                return;
            }
            let dir = (b - a) / len;
            let perp = vec2(-dir.y, dir.x);
            let head = (w * 4.0 + 6.0 * s).min(len);
            let base = b - dir * head;
            out.push(Shape::line_segment([a, base + dir * (head * 0.3)], Stroke::new(w, *color)));
            out.push(Shape::convex_polygon(vec![b, base + perp * head * 0.55, base - perp * head * 0.55], *color, Stroke::NONE));
        }
        Mark::Rect { rect, color, width, fill } => {
            let r = Rect::from_two_pos(view.to_screen(rect.min), view.to_screen(rect.max));
            let w = width * s;
            match fill {
                // Out to where the outline would end.
                Fill::Solid => out.push(Shape::rect_filled(r.expand(w / 2.0), CornerRadius::ZERO, *color)),
                Fill::Tint => {
                    let [cr, cg, cb, _] = color.to_srgba_unmultiplied();
                    out.push(Shape::rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(cr, cg, cb, TINT_ALPHA)));
                    out.push(Shape::rect_stroke(r, CornerRadius::ZERO, Stroke::new(w, *color), StrokeKind::Middle));
                }
                Fill::None => out.push(Shape::rect_stroke(r, CornerRadius::ZERO, Stroke::new(w, *color), StrokeKind::Middle)),
            }
        }
        Mark::Text { pos, text, color, size } => {
            let g = painter.layout_no_wrap(text.clone(), FontId::proportional(size * s), *color);
            out.push(Shape::galley(view.to_screen(*pos), g, *color));
        }
        Mark::Pixelate { .. } => {} // drawn from the image's pixels: see `pixel_shape`
    }
}

/// Averaged squares of the image over a rectangle (image pixels).
struct Blocks {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    size: usize,
    cols: usize,
    colors: Vec<[u8; 3]>,
}

impl Blocks {
    fn new(rgba: &[u8], width: u32, rect: Rect, block: f32) -> Option<Blocks> {
        let height = (rgba.len() / 4 / width as usize) as f32;
        let r = round_rect(rect).intersect(Rect::from_min_size(Pos2::ZERO, vec2(width as f32, height)));
        if r.width() < 1.0 || r.height() < 1.0 {
            return None;
        }
        let (x0, y0, x1, y1) = (r.min.x as usize, r.min.y as usize, r.max.x as usize, r.max.y as usize);
        let size = (block.round() as usize).max(2);
        let (cols, rows) = ((x1 - x0).div_ceil(size), (y1 - y0).div_ceil(size));
        let mut colors = Vec::with_capacity(cols * rows);
        for by in 0..rows {
            for bx in 0..cols {
                let (sx, sy) = (x0 + bx * size, y0 + by * size);
                let (ex, ey) = ((sx + size).min(x1), (sy + size).min(y1));
                let mut sum = [0u64; 3];
                for y in sy..ey {
                    for x in sx..ex {
                        let p = (y * width as usize + x) * 4;
                        for k in 0..3 {
                            sum[k] += rgba[p + k] as u64;
                        }
                    }
                }
                let n = ((ex - sx) * (ey - sy)) as u64;
                colors.push(sum.map(|v| (v / n) as u8));
            }
        }
        Some(Blocks { x0, y0, x1, y1, size, cols, colors })
    }
}

impl Tool {
    /// The tool, as named in sentences.
    fn noun(self) -> &'static str {
        match self {
            Tool::Pen => "pen",
            Tool::Highlighter => "highlighter",
            Tool::Arrow => "arrow",
            Tool::Rect => "rectangle",
            Tool::Text => "text",
            Tool::Pixelate => "pixelation",
            _ => "mark",
        }
    }

    fn noun_capitalized(self) -> String {
        let n = self.noun();
        n[..1].to_uppercase() + &n[1..]
    }
}

/// `m`'s color, if it has one.
fn mark_color(m: &Mark) -> Option<Color32> {
    match m {
        Mark::Line { color, .. } | Mark::Arrow { color, .. } | Mark::Rect { color, .. } | Mark::Text { color, .. } => Some(*color),
        Mark::Pixelate { .. } => None,
    }
}

/// `c` without its see-through (a highlighter's).
fn opaque(c: Color32) -> Color32 {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Color32::from_rgb(r, g, b)
}

/// Which palette color `c` is, if any (near, not equal: a see-through color comes back a little off).
fn palette_index(c: Color32) -> Option<usize> {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    let near = |x: u8, y: u8| x.abs_diff(y) <= 3;
    COLORS.iter().position(|p| near(p.r(), r) && near(p.g(), g) && near(p.b(), b))
}

/// Encodes the output: JPEG at the set quality, or a compact PNG (reduced to 256 colors if set).
fn encode_output(w: u32, h: u32, rgba: &[u8], jpeg: bool, opts: Output) -> Result<Vec<u8>, String> {
    if jpeg {
        if w > u16::MAX as u32 || h > u16::MAX as u32 {
            return Err("too large for JPEG".into());
        }
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, opts.jpeg_quality)
            .encode(rgba, w as u16, h as u16, jpeg_encoder::ColorType::Rgba)
            .map_err(|e| e.to_string())?;
        Ok(out)
    } else if opts.reduce_colors {
        crate::screenshot::encode_png(w, h, &reduce_colors(rgba), crate::screenshot::Order::Rgba)
    } else {
        crate::screenshot::encode_png(w, h, rgba, crate::screenshot::Order::Rgba)
    }
}

fn kb(bytes: usize) -> String {
    if bytes >= 1 << 20 { format!("{:.1} MB", bytes as f64 / (1 << 20) as f64) } else { format!("{} KB", bytes.div_ceil(1024)) }
}

/// Resizes an RGBA image by `scale` (1 = as is): shrinking averages, upscaling as `method` says.
fn resize_image(w: u32, h: u32, rgba: Vec<u8>, scale: f32, method: Upscale) -> (u32, u32, Vec<u8>) {
    if scale <= 1.0 {
        return shrink(w, h, rgba, scale);
    }
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    match method {
        Upscale::Pixels => (nw, nh, nearest(w, h, &rgba, nw, nh)),
        Upscale::Sharp => (nw, nh, sharpen(nw, nh, &lanczos(w, h, &rgba, nw, nh), 0.5)),
    }
}

/// Each output pixel takes the input pixel it falls on.
fn nearest(w: u32, h: u32, rgba: &[u8], nw: u32, nh: u32) -> Vec<u8> {
    let (w, h, nw, nh) = (w as usize, h as usize, nw as usize, nh as usize);
    let xs: Vec<usize> = (0..nw).map(|x| (x * w / nw).min(w - 1)).collect();
    let mut out = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        let row = (y * h / nh).min(h - 1) * w;
        for &x in &xs {
            out.extend_from_slice(&rgba[(row + x) * 4..(row + x) * 4 + 4]);
        }
    }
    out
}

/// Lanczos-3 resampling, one axis at a time.
fn lanczos(w: u32, h: u32, rgba: &[u8], nw: u32, nh: u32) -> Vec<u8> {
    fn kernel(x: f32) -> f32 {
        if x.abs() < 1e-6 {
            1.0
        } else if x.abs() >= 3.0 {
            0.0
        } else {
            let px = std::f32::consts::PI * x;
            3.0 * px.sin() * (px / 3.0).sin() / (px * px)
        }
    }
    // For each output position along an axis: the first input index and the normalized weights.
    fn taps(n: usize, nn: usize) -> Vec<(isize, [f32; 6])> {
        (0..nn)
            .map(|o| {
                let center = (o as f32 + 0.5) * n as f32 / nn as f32 - 0.5;
                let first = center.floor() as isize - 2;
                let mut wt = [0.0f32; 6];
                for (k, w) in wt.iter_mut().enumerate() {
                    *w = kernel(center - (first + k as isize) as f32);
                }
                let sum: f32 = wt.iter().sum();
                (first, wt.map(|w| w / sum))
            })
            .collect()
    }
    let (w, h, nw, nh) = (w as usize, h as usize, nw as usize, nh as usize);
    let clamp = |i: isize, n: usize| i.clamp(0, n as isize - 1) as usize;
    // Across: h rows of nw pixels, as floats.
    let tx = taps(w, nw);
    let mut mid = vec![0.0f32; h * nw * 4];
    for y in 0..h {
        for (x, (first, wt)) in tx.iter().enumerate() {
            let mut acc = [0.0f32; 4];
            for (k, wk) in wt.iter().enumerate() {
                let i = (y * w + clamp(first + k as isize, w)) * 4;
                for c in 0..4 {
                    acc[c] += rgba[i + c] as f32 * wk;
                }
            }
            mid[(y * nw + x) * 4..(y * nw + x) * 4 + 4].copy_from_slice(&acc);
        }
    }
    // Down.
    let ty = taps(h, nh);
    let mut out = vec![0u8; nw * nh * 4];
    for (y, (first, wt)) in ty.iter().enumerate() {
        for x in 0..nw {
            let mut acc = [0.0f32; 4];
            for (k, wk) in wt.iter().enumerate() {
                let i = (clamp(first + k as isize, h) * nw + x) * 4;
                for c in 0..4 {
                    acc[c] += mid[i + c] * wk;
                }
            }
            for c in 0..4 {
                out[(y * nw + x) * 4 + c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Unsharp mask: adds `amount` of the difference from a 3×3 blur, crisping edges. Alpha is kept.
fn sharpen(w: u32, h: u32, rgba: &[u8], amount: f32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = rgba.to_vec();
    if w < 3 || h < 3 {
        return out;
    }
    const K: [[f32; 3]; 3] = [[1.0, 2.0, 1.0], [2.0, 4.0, 2.0], [1.0, 2.0, 1.0]];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            for c in 0..3 {
                let mut blur = 0.0;
                for (dy, row) in K.iter().enumerate() {
                    for (dx, k) in row.iter().enumerate() {
                        blur += rgba[((y + dy - 1) * w + x + dx - 1) * 4 + c] as f32 * k;
                    }
                }
                let v = rgba[(y * w + x) * 4 + c] as f32;
                out[(y * w + x) * 4 + c] = (v + amount * (v - blur / 16.0)).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

fn decode_jpeg(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = jpeg_decoder::Decoder::new(data);
    let px = dec.decode().ok()?;
    match dec.info()?.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => Some(px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()),
        jpeg_decoder::PixelFormat::L8 => Some(px.iter().flat_map(|&g| [g, g, g, 255]).collect()),
        _ => None,
    }
}

/// Shrinks an RGBA image by `scale` (≤ 1): each output pixel is the average of the pixels it covers, which
/// keeps text readable.
fn shrink(w: u32, h: u32, rgba: Vec<u8>, scale: f32) -> (u32, u32, Vec<u8>) {
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    if (nw, nh) == (w, h) {
        return (w, h, rgba);
    }
    let (w, h, nw, nh) = (w as usize, h as usize, nw as usize, nh as usize);
    let mut out = Vec::with_capacity(nw * nh * 4);
    for oy in 0..nh {
        let (y0, y1) = (oy * h / nh, ((oy + 1) * h / nh).max(oy * h / nh + 1));
        for ox in 0..nw {
            let (x0, x1) = (ox * w / nw, ((ox + 1) * w / nw).max(ox * w / nw + 1));
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for p in rgba[(y * w + x0) * 4..(y * w + x1) * 4].chunks_exact(4) {
                    for k in 0..4 {
                        sum[k] += p[k] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            out.extend(sum.map(|v| ((v + n / 2) / n) as u8));
        }
    }
    (nw as u32, nh as u32, out)
}

/// The image in at most 256 colors (NeuQuant), so its PNG can use a palette. Images that already have that
/// few come back as they are.
fn reduce_colors(rgba: &[u8]) -> Vec<u8> {
    let mut seen = std::collections::HashSet::new();
    let few = rgba.chunks_exact(4).all(|p| {
        seen.insert([p[0], p[1], p[2]]);
        seen.len() <= 256
    });
    if few {
        return rgba.to_vec();
    }
    let opaque: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
    let nq = color_quant::NeuQuant::new(10, 256, &opaque);
    let map = nq.color_map_rgba();
    opaque.chunks_exact(4).flat_map(|p| {
        let i = nq.index_of(p) * 4;
        [map[i], map[i + 1], map[i + 2], 255]
    })
    .collect()
}

/// `#RRGGBB` or `RRGGBB` as a color.
fn parse_hex(text: &str) -> Option<Color32> {
    let t = text.trim().trim_start_matches('#');
    if t.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(t, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn hsv_to_rgb([h, s, v]: [f32; 3]) -> Color32 {
    let h6 = (h.rem_euclid(1.0)) * 6.0;
    let c = v * s;
    let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
    let (r, g, b) = match h6 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let u = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(u(r), u(g), u(b))
}

/// `c` as hue, saturation, value; where those are undefined (grey, black) `keep`'s are kept.
fn rgb_to_hsv(c: Color32, keep: [f32; 3]) -> [f32; 3] {
    let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    if max == 0.0 {
        return [keep[0], keep[1], 0.0];
    }
    let s = d / max;
    if d == 0.0 {
        return [keep[0], s, max];
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    [h, s, max]
}

/// Saturation (across) and brightness (down) at the current hue. Returns whether it changed.
fn color_square(ui: &mut egui::Ui, hsv: &mut [f32; 3]) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 150.0), Sense::click_and_drag());
    let mut changed = false;
    if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.is_pointer_button_down_on()) {
        hsv[1] = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        hsv[2] = 1.0 - ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        changed = true;
    }
    // A grid of quads: corner colors interpolate across each, close enough to the exact gradient.
    const N: usize = 12;
    let mut mesh = Mesh::default();
    for j in 0..=N {
        for i in 0..=N {
            let (s, v) = (i as f32 / N as f32, 1.0 - j as f32 / N as f32);
            let pos = pos2(rect.left() + rect.width() * s, rect.top() + rect.height() * (1.0 - v));
            mesh.colored_vertex(pos, hsv_to_rgb([hsv[0], s, v]));
        }
    }
    for j in 0..N as u32 {
        for i in 0..N as u32 {
            let a = j * (N as u32 + 1) + i;
            let b = a + N as u32 + 1;
            mesh.add_triangle(a, a + 1, b);
            mesh.add_triangle(a + 1, b + 1, b);
        }
    }
    let p = ui.painter_at(rect);
    p.add(Shape::mesh(mesh));
    let at = pos2(rect.left() + rect.width() * hsv[1], rect.top() + rect.height() * (1.0 - hsv[2]));
    p.circle_stroke(at, 6.0, Stroke::new(2.0, Color32::WHITE));
    p.circle_stroke(at, 7.5, Stroke::new(1.0, Color32::from_black_alpha(150)));
    changed
}

/// The hue, left to right. Returns whether it changed.
fn hue_bar(ui: &mut egui::Ui, hsv: &mut [f32; 3]) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::click_and_drag());
    let mut changed = false;
    if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.is_pointer_button_down_on()) {
        hsv[0] = ((p.x - rect.left()) / rect.width()).clamp(0.0, 0.9999);
        changed = true;
    }
    const N: u32 = 36;
    let mut mesh = Mesh::default();
    for i in 0..=N {
        let h = i as f32 / N as f32;
        let x = rect.left() + rect.width() * h;
        let c = hsv_to_rgb([h, 1.0, 1.0]);
        mesh.colored_vertex(pos2(x, rect.top()), c);
        mesh.colored_vertex(pos2(x, rect.bottom()), c);
    }
    for i in 0..N {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    let p = ui.painter_at(rect.expand(3.0));
    p.add(Shape::mesh(mesh));
    let x = rect.left() + rect.width() * hsv[0];
    let knob = Rect::from_center_size(pos2(x, rect.center().y), vec2(6.0, rect.height() + 4.0));
    p.rect(knob, CornerRadius::same(2), hsv_to_rgb([hsv[0], 1.0, 1.0]), Stroke::new(2.0, Color32::WHITE), StrokeKind::Middle);
    changed
}

fn hex(c: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

/// The color picker's preview by the pointer: the color under it and its hex code.
fn loupe(painter: &egui::Painter, at: Pos2, c: Color32) {
    let center = at + vec2(30.0, 30.0);
    painter.circle(center, 18.0, c, Stroke::new(2.0, Color32::WHITE));
    painter.circle_stroke(center, 19.5, Stroke::new(1.0, Color32::from_black_alpha(160)));
    let g = painter.layout_no_wrap(format!("{}  (click copies, Shift+click draws)", hex(c)), FontId::proportional(12.0), TEXT);
    let pill = Rect::from_center_size(center + vec2(0.0, 32.0), g.size() + vec2(12.0, 6.0));
    painter.rect_filled(pill, CornerRadius::same(4), Color32::from_rgb(0x2B, 0x2B, 0x2B));
    painter.galley(pill.min + vec2(6.0, 3.0), g, TEXT);
}

/// The toolbar's color button: the current color as a round swatch.
fn color_button(ui: &mut egui::Ui, color: Color32, open: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 36.0), Sense::click());
    let p = ui.painter();
    if open {
        p.rect_filled(rect, CornerRadius::same(4), fluent::CONTROL_HOVER);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(4), fluent::CONTROL_PRESSED);
    }
    p.circle_filled(rect.center(), 10.0, color);
    p.text(pos2(rect.right() - 5.0, rect.bottom() - 7.0), Align2::CENTER_CENTER, "\u{E70D}", FontId::proportional(8.0), TEXT3);
    resp
}

/// The tool that makes marks like `m` (whose color and sizes apply to it).
fn mark_tool(m: &Mark) -> Tool {
    match m {
        Mark::Line { color, .. } if color.a() < 255 => Tool::Highlighter,
        Mark::Line { .. } => Tool::Pen,
        Mark::Arrow { .. } => Tool::Arrow,
        Mark::Rect { .. } => Tool::Rect,
        Mark::Text { .. } => Tool::Text,
        Mark::Pixelate { .. } => Tool::Pixelate,
    }
}

/// `m`'s color (index into COLORS) and size (index into `kind`'s sizes, the nearest).
fn style_of(m: &Mark, kind: Tool) -> (Option<usize>, usize) {
    let (color, size) = match m {
        Mark::Line { color, width, .. } | Mark::Arrow { color, width, .. } | Mark::Rect { color, width, .. } => (Some(*color), *width),
        Mark::Text { color, size, .. } => (Some(*color), *size),
        Mark::Pixelate { block, .. } => (None, *block),
    };
    let index = color.and_then(palette_index);
    let sizes = kind.sizes();
    let nearest = (0..3).min_by(|a, b| (sizes[*a] - size).abs().total_cmp(&(sizes[*b] - size).abs())).unwrap_or(1);
    (index, nearest)
}

/// Gives `m` a new color (keeping a highlighter see-through) and/or size.
fn restyle(m: &mut Mark, color: Option<Color32>, size: Option<f32>) {
    let recolor = |old: Color32, new: Color32| {
        if old.a() < 255 { Color32::from_rgba_unmultiplied(new.r(), new.g(), new.b(), HIGHLIGHT_ALPHA) } else { new }
    };
    match m {
        Mark::Line { color: c, width, .. } | Mark::Arrow { color: c, width, .. } | Mark::Rect { color: c, width, .. } => {
            if let Some(n) = color {
                *c = recolor(*c, n);
            }
            if let Some(z) = size {
                *width = z;
            }
        }
        Mark::Text { color: c, size: s, .. } => {
            if let Some(n) = color {
                *c = n;
            }
            if let Some(z) = size {
                *s = z;
            }
        }
        Mark::Pixelate { block, .. } => {
            if let Some(z) = size {
                *block = z;
            }
        }
    }
}

fn translate(m: &mut Mark, d: Vec2) {
    match m {
        Mark::Line { points, .. } => points.iter_mut().for_each(|p| *p += d),
        Mark::Arrow { from, to, .. } => {
            *from += d;
            *to += d;
        }
        Mark::Rect { rect, .. } | Mark::Pixelate { rect, .. } => *rect = rect.translate(d),
        Mark::Text { pos, .. } => *pos += d,
    }
}

/// The area `m` covers, in image pixels.
fn mark_bounds(m: &Mark, painter: &egui::Painter) -> Rect {
    match m {
        Mark::Line { points, width, .. } => Rect::from_points(points).expand(width / 2.0),
        Mark::Arrow { from, to, width, .. } => Rect::from_two_pos(*from, *to).expand(width * 2.0),
        Mark::Rect { rect, width, .. } => rect.expand(width / 2.0),
        Mark::Pixelate { rect, .. } => *rect,
        Mark::Text { pos, text, size, .. } => {
            let g = painter.layout_no_wrap(text.clone(), FontId::proportional(*size), Color32::WHITE);
            Rect::from_min_size(*pos, g.size())
        }
    }
}

/// `m` with a freehand line smoothed (other marks as they are).
fn smoothed(m: &Mark, level: Smoothing) -> Mark {
    match m {
        Mark::Line { points, color, width } => Mark::Line { points: smooth(points, *width, level), color: *color, width: *width },
        other => other.clone(),
    }
}

/// Evens out a freehand stroke: a moving average takes out the hand's jitter, then corner cutting (Chaikin)
/// rounds what's left into curves. The ends stay where they were drawn. `level` sets how wide the average is
/// and how many times corners are cut.
fn smooth(points: &[Pos2], width: f32, level: Smoothing) -> Vec<Pos2> {
    let n = points.len();
    let (radius, passes) = match level {
        Smoothing::Off => return points.to_vec(),
        Smoothing::Low => (1, 1),
        Smoothing::Medium => (3, 2),
        Smoothing::High => (7, 3),
    };
    if n < 3 {
        return points.to_vec();
    }
    let mut pts: Vec<Pos2> = (0..n)
        .map(|i| {
            let r = radius.min(i).min(n - 1 - i); // narrower toward the ends, which stay put
            let sum = points[i - r..=i + r].iter().fold(Vec2::ZERO, |a, p| a + p.to_vec2());
            (sum / (2 * r + 1) as f32).to_pos2()
        })
        .collect();
    for _ in 0..passes {
        let mut out = Vec::with_capacity(pts.len() * 2);
        out.push(pts[0]);
        for w in pts.windows(2) {
            out.push(w[0].lerp(w[1], 0.25));
            out.push(w[0].lerp(w[1], 0.75));
        }
        out.push(pts[pts.len() - 1]);
        pts = out;
    }
    // Thin out points closer than a fraction of the width: they add nothing visible.
    let min = (width * 0.2).max(0.5);
    let last = pts[pts.len() - 1];
    let mut thin = vec![pts[0]];
    for p in &pts[1..pts.len() - 1] {
        if thin[thin.len() - 1].distance(*p) >= min {
            thin.push(*p);
        }
    }
    thin.push(last);
    thin
}

fn is_empty(m: &Mark) -> bool {
    match m {
        Mark::Line { points, .. } => points.is_empty(),
        Mark::Arrow { from, to, .. } => from.distance(*to) < 2.0,
        Mark::Rect { rect, .. } => rect.width() < 2.0 && rect.height() < 2.0,
        Mark::Text { text, .. } => text.trim().is_empty(),
        Mark::Pixelate { rect, .. } => rect.width() < 2.0 || rect.height() < 2.0,
    }
}

/// Whether `p` (image pixels) is on mark `m`, within `slack`.
fn hits(m: &Mark, p: Pos2, slack: f32, painter: &egui::Painter) -> bool {
    match m {
        Mark::Line { points, width, .. } => {
            let r = width / 2.0 + slack;
            points.len() == 1 && points[0].distance(p) <= r || points.windows(2).any(|w| seg_dist(p, w[0], w[1]) <= r)
        }
        Mark::Arrow { from, to, width, .. } => seg_dist(p, *from, *to) <= width * 2.0 + slack,
        Mark::Rect { rect, width, fill, .. } if *fill != Fill::None => rect.expand(width / 2.0 + slack).contains(p),
        Mark::Rect { rect, width, .. } => {
            let r = width / 2.0 + slack;
            let c = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom()];
            (0..4).any(|i| seg_dist(p, c[i], c[(i + 1) % 4]) <= r)
        }
        Mark::Text { pos, text, size, .. } => {
            let g = painter.layout_no_wrap(text.clone(), FontId::proportional(*size), Color32::WHITE);
            Rect::from_min_size(*pos, g.size()).expand(slack).contains(p)
        }
        Mark::Pixelate { rect, .. } => rect.expand(slack).contains(p),
    }
}

fn seg_dist(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

/// The corner of `r` opposite the one being dragged to `p` (for a rectangle drawn from a fixed corner).
fn rect_anchor(r: Rect, p: Pos2) -> Pos2 {
    let x = if (p.x - r.min.x).abs() < (p.x - r.max.x).abs() { r.max.x } else { r.min.x };
    let y = if (p.y - r.min.y).abs() < (p.y - r.max.y).abs() { r.max.y } else { r.min.y };
    pos2(x, y)
}

fn round_rect(r: Rect) -> Rect {
    Rect::from_min_max(r.min.round(), r.max.round())
}

fn clamp_pos(p: Pos2, r: Rect) -> Pos2 {
    pos2(p.x.clamp(r.min.x, r.max.x), p.y.clamp(r.min.y, r.max.y))
}

/// Which part of crop `r` is at `p`: `Some((0, 0))` inside, `Some((sx, sy))` an edge or corner, None outside.
fn crop_hit(r: Rect, p: Pos2, margin: f32) -> Option<(i8, i8)> {
    if !r.expand(margin).contains(p) {
        return None;
    }
    let near = |v: f32, e: f32| (v - e).abs() <= margin;
    let sx = if near(p.x, r.min.x) { -1 } else if near(p.x, r.max.x) { 1 } else { 0 };
    let sy = if near(p.y, r.min.y) { -1 } else if near(p.y, r.max.y) { 1 } else { 0 };
    Some((sx, sy))
}

fn resize(orig: Rect, sx: i8, sy: i8, p: Pos2) -> Rect {
    const MIN: f32 = 4.0;
    let mut r = orig;
    match sx {
        -1 => r.min.x = p.x.min(r.max.x - MIN),
        1 => r.max.x = p.x.max(r.min.x + MIN),
        _ => {}
    }
    match sy {
        -1 => r.min.y = p.y.min(r.max.y - MIN),
        1 => r.max.y = p.y.max(r.min.y + MIN),
        _ => {}
    }
    r
}

// ---- rasterizing ----

/// Fills `mesh` (positions in points, `scale` pixels per point) into the RGBA image `buf`, blending its
/// premultiplied colors over it, textured from the font `atlas` as egui's GPU painter would.
fn raster(buf: &mut [u8], w: usize, h: usize, mesh: &Mesh, scale: f32, atlas: &ColorImage) {
    let [aw, ah] = atlas.size;
    let texel = |x: isize, y: isize| {
        let c = atlas.pixels[y.clamp(0, ah as isize - 1) as usize * aw + x.clamp(0, aw as isize - 1) as usize];
        [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32]
    };
    let sample = |uv: Pos2| {
        let (x, y) = (uv.x * aw as f32 - 0.5, uv.y * ah as f32 - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let (a, b, c, d) = (texel(x0, y0), texel(x0 + 1, y0), texel(x0, y0 + 1), texel(x0 + 1, y0 + 1));
        std::array::from_fn::<f32, 4, _>(|k| (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy)
    };
    let edge = |a: Pos2, b: Pos2, p: Pos2| (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
    for tri in mesh.indices.chunks_exact(3) {
        let mut v = [tri[0], tri[1], tri[2]].map(|i| mesh.vertices[i as usize]);
        let mut p = v.map(|v| pos2(v.pos.x * scale, v.pos.y * scale));
        let mut area = edge(p[0], p[1], p[2]);
        if area.abs() < 1e-6 {
            continue;
        }
        if area < 0.0 {
            v.swap(1, 2);
            p.swap(1, 2);
            area = -area;
        }
        let x0 = p.iter().map(|q| q.x).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let y0 = p.iter().map(|q| q.y).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let x1 = (p.iter().map(|q| q.x).fold(f32::MIN, f32::max).ceil() as isize).min(w as isize - 1);
        let y1 = (p.iter().map(|q| q.y).fold(f32::MIN, f32::max).ceil() as isize).min(h as isize - 1);
        if x1 < 0 || y1 < 0 {
            continue;
        }
        let col = v.map(|v| [v.color.r() as f32, v.color.g() as f32, v.color.b() as f32, v.color.a() as f32]);
        for y in y0..=y1 as usize {
            for x in x0..=x1 as usize {
                let c = pos2(x as f32 + 0.5, y as f32 + 0.5);
                let (e0, e1, e2) = (edge(p[1], p[2], c), edge(p[2], p[0], c), edge(p[0], p[1], c));
                // Shared edges count for one triangle only, so translucent fills don't double up along them.
                if e0 < 0.0 || e1 < 0.0 || e2 < 0.0 || (e0 == 0.0 && !top_left(p[1], p[2])) || (e1 == 0.0 && !top_left(p[2], p[0])) || (e2 == 0.0 && !top_left(p[0], p[1])) {
                    continue;
                }
                let (b0, b1, b2) = (e0 / area, e1 / area, e2 / area);
                let uv = pos2(
                    v[0].uv.x * b0 + v[1].uv.x * b1 + v[2].uv.x * b2,
                    v[0].uv.y * b0 + v[1].uv.y * b1 + v[2].uv.y * b2,
                );
                let t = sample(uv);
                let src: [f32; 4] = std::array::from_fn(|k| (col[0][k] * b0 + col[1][k] * b1 + col[2][k] * b2) * t[k] / 255.0);
                let keep = 1.0 - src[3] / 255.0;
                let d = &mut buf[(y * w + x) * 4..(y * w + x) * 4 + 3];
                for k in 0..3 {
                    d[k] = (src[k] + d[k] as f32 * keep).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

/// Top-left fill rule for a clockwise (in y-down screen space) triangle's edge a→b.
fn top_left(a: Pos2, b: Pos2) -> bool {
    (a.y == b.y && b.x > a.x) || b.y < a.y
}

/// A tool's button: its glyph; when `selected` a raised look, and a chevron under it if it `has_options` (its
/// popup).
fn tool_button(ui: &mut egui::Ui, glyph: &str, selected: bool, has_options: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 36.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(4), fluent::CONTROL_HOVER);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(4), fluent::CONTROL_PRESSED);
    }
    let c = rect.center() - vec2(0.0, 2.0);
    p.text(c, Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), if selected { TEXT } else { TEXT2 });
    if selected && has_options {
        p.text(pos2(c.x, rect.bottom() - 4.5), Align2::CENTER_CENTER, "\u{E70D}", FontId::proportional(7.0), TEXT3);
    }
    resp
}

fn plain_button(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        let fill = if resp.is_pointer_button_down_on() { fluent::CONTROL_PRESSED } else { fluent::CONTROL_HOVER };
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(16.0), if enabled { TEXT } else { TEXT3 });
    resp.on_hover_text(tip)
}

fn swatch(ui: &mut egui::Ui, color: Color32, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 36.0), Sense::click());
    let c = rect.center();
    let p = ui.painter();
    if selected {
        p.circle_stroke(c, 12.0, Stroke::new(2.0, TEXT));
    } else if resp.hovered() {
        p.circle_stroke(c, 12.0, Stroke::new(1.0, TEXT3));
    }
    p.circle(c, 9.0, color, Stroke::new(1.0, Color32::from_white_alpha(60)));
    resp
}

fn size_button(ui: &mut egui::Ui, k: usize, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 36.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), CornerRadius::same(4), fluent::CONTROL_HOVER);
    } else if resp.hovered() {
        p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), CornerRadius::same(4), fluent::CONTROL_PRESSED);
    }
    p.circle_filled(rect.center(), [2.5, 4.5, 7.0][k], if selected { TEXT } else { TEXT2 });
    resp
}

/// An outline, tinted or filled square: the rectangle fill choices.
fn fill_button(ui: &mut egui::Ui, fill: Fill, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 36.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), CornerRadius::same(4), fluent::CONTROL_HOVER);
    } else if resp.hovered() {
        p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), CornerRadius::same(4), fluent::CONTROL_PRESSED);
    }
    let color = if selected { TEXT } else { TEXT2 };
    let sq = Rect::from_center_size(rect.center(), Vec2::splat(14.0));
    match fill {
        Fill::Solid => {
            p.rect_filled(sq, CornerRadius::same(2), color);
        }
        _ => {
            if fill == Fill::Tint {
                p.rect_filled(sq, CornerRadius::same(2), color.gamma_multiply(0.35));
            }
            p.rect_stroke(sq, CornerRadius::same(2), Stroke::new(1.5, color), StrokeKind::Inside);
        }
    }
    resp
}

fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 36.0), Sense::hover());
    ui.painter().line_segment([pos2(rect.center().x, rect.top() + 8.0), pos2(rect.center().x, rect.bottom() - 8.0)], Stroke::new(1.0, Color32::from_white_alpha(30)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::Vertex;

    fn solid_atlas() -> ColorImage {
        ColorImage::filled([4, 4], Color32::WHITE)
    }

    #[test]
    fn rasterizes_a_square_without_double_blending_the_diagonal() {
        // Two triangles making a 4×4 square at half-transparent red, over black.
        let red = Color32::from_rgba_unmultiplied(255, 0, 0, 128);
        let v = |x: f32, y: f32| Vertex { pos: pos2(x, y), uv: Pos2::ZERO, color: red };
        let mesh = Mesh { indices: vec![0, 1, 2, 0, 2, 3], vertices: vec![v(0.0, 0.0), v(4.0, 0.0), v(4.0, 4.0), v(0.0, 4.0)], ..Default::default() };
        let mut buf = vec![0u8; 6 * 6 * 4];
        raster(&mut buf, 6, 6, &mesh, 1.0, &solid_atlas());
        let red_at = |x: usize, y: usize| buf[(y * 6 + x) * 4];
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(red_at(x, y), red.r(), "pixel {x},{y}");
            }
        }
        assert_eq!(red_at(4, 4), 0);
        assert_eq!(red_at(5, 0), 0);
    }

    #[test]
    fn scales_points_to_pixels() {
        let v = |x: f32, y: f32| Vertex { pos: pos2(x, y), uv: Pos2::ZERO, color: Color32::WHITE };
        let mesh = Mesh { indices: vec![0, 1, 2, 0, 2, 3], vertices: vec![v(0.0, 0.0), v(2.0, 0.0), v(2.0, 2.0), v(0.0, 2.0)], ..Default::default() };
        let mut buf = vec![0u8; 8 * 8 * 4];
        raster(&mut buf, 8, 8, &mesh, 2.0, &solid_atlas()); // 2 points = 4 pixels
        assert_eq!(buf[(3 * 8 + 3) * 4], 255);
        assert_eq!(buf[(4 * 8 + 4) * 4], 0);
    }

    #[test]
    fn erases_by_distance() {
        let line = Mark::Line { points: vec![pos2(0.0, 0.0), pos2(100.0, 0.0)], color: Color32::RED, width: 4.0 };
        assert!(seg_dist(pos2(50.0, 3.0), pos2(0.0, 0.0), pos2(100.0, 0.0)) < 4.0);
        let Mark::Line { points, .. } = &line else { unreachable!() };
        assert_eq!(points.len(), 2);
        assert!(seg_dist(pos2(150.0, 0.0), pos2(0.0, 0.0), pos2(100.0, 0.0)) > 49.0);
    }

    #[test]
    fn pixelates_in_averaged_blocks() {
        // 4×2 image: left half black, right half white; 2-pixel blocks.
        let rgba: Vec<u8> = (0..8).flat_map(|i| if i % 4 < 2 { [0, 0, 0, 255] } else { [255, 255, 255, 255] }).collect();
        let b = Blocks::new(&rgba, 4, Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 2.0)), 2.0).unwrap();
        assert_eq!((b.cols, b.colors.len()), (2, 2));
        assert_eq!(b.colors, vec![[0, 0, 0], [255, 255, 255]]);
        let mixed = Blocks::new(&rgba, 4, Rect::from_min_max(pos2(1.0, 0.0), pos2(3.0, 2.0)), 2.0).unwrap();
        assert_eq!(mixed.colors, vec![[127, 127, 127]]);
    }

    #[test]
    fn restyles_and_moves_marks() {
        let mut m = Mark::Line { points: vec![pos2(0.0, 0.0)], color: Color32::from_rgba_unmultiplied(253, 216, 53, HIGHLIGHT_ALPHA), width: 20.0 };
        assert_eq!(mark_tool(&m), Tool::Highlighter);
        assert_eq!(style_of(&m, Tool::Highlighter), (Some(2), 1));
        restyle(&mut m, Some(COLORS[0]), Some(32.0));
        assert_eq!(style_of(&m, Tool::Highlighter), (Some(0), 2));
        assert_eq!(mark_tool(&m), Tool::Highlighter, "still see-through");
        translate(&mut m, vec2(5.0, -1.0));
        let Mark::Line { points, .. } = &m else { unreachable!() };
        assert_eq!(points[0], pos2(5.0, -1.0));
    }

    #[test]
    fn smoothing_keeps_the_ends_and_removes_jitter() {
        // A straight line with ±2 px zigzag.
        let raw: Vec<Pos2> = (0..60).map(|i| pos2(i as f32 * 3.0, if i % 2 == 0 { 2.0 } else { -2.0 })).collect();
        let s = smooth(&raw, 4.0, Smoothing::Medium);
        assert_eq!(smooth(&raw, 4.0, Smoothing::Off), raw);
        let high = smooth(&raw, 4.0, Smoothing::High);
        assert_eq!((high[0], high[high.len() - 1]), (raw[0], raw[raw.len() - 1]));
        assert_eq!(s[0], raw[0]);
        assert_eq!(s[s.len() - 1], raw[raw.len() - 1]);
        let inner = &s[s.len() / 4..s.len() * 3 / 4];
        assert!(inner.iter().all(|p| p.y.abs() < 0.6), "jitter left: {:?}", inner.iter().map(|p| p.y).fold(0.0f32, |a, y| a.max(y.abs())));
    }

    #[test]
    fn filled_rectangles_are_hit_inside() {
        let painter_free = |fill| Mark::Rect { rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0)), color: Color32::RED, width: 4.0, fill };
        let ctx = egui::Context::default();
        let painter = egui::Painter::new(ctx, egui::LayerId::background(), Rect::EVERYTHING);
        assert!(!hits(&painter_free(Fill::None), pos2(50.0, 25.0), 2.0, &painter));
        assert!(hits(&painter_free(Fill::Solid), pos2(50.0, 25.0), 2.0, &painter));
        assert!(hits(&painter_free(Fill::None), pos2(50.0, 1.0), 2.0, &painter));
    }

    #[test]
    fn color_conversions_round_trip() {
        for c in COLORS.into_iter().chain([Color32::from_rgb(127, 97, 82), Color32::from_rgb(10, 200, 77)]) {
            assert_eq!(hsv_to_rgb(rgb_to_hsv(c, [0.0, 0.0, 0.0])), c);
        }
        // Grey keeps the hue it had.
        assert_eq!(rgb_to_hsv(Color32::from_gray(128), [0.3, 0.5, 0.5])[0], 0.3);
        assert_eq!(parse_hex("#1E88E5"), Some(Color32::from_rgb(0x1E, 0x88, 0xE5)));
        assert_eq!(parse_hex("1e88e5"), Some(Color32::from_rgb(0x1E, 0x88, 0xE5)));
        assert_eq!(parse_hex("#1E88E"), None);
    }

    #[test]
    fn tool_memory_survives_json() {
        let m = ToolMemory {
            tool: "Highlighter".into(),
            styles: vec![("Pen".into(), "#1E88E5".into(), 2), ("Rect".into(), "#43A047".into(), 0)],
            rect_fill: "Tint".into(),
            output: Output { scale: 0.5, upscale: Upscale::Pixels, jpeg: true, reduce_colors: true, jpeg_quality: 75 },
        };
        let back: ToolMemory = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        // Older or partial files still load.
        let partial: ToolMemory = serde_json::from_str(r#"{"tool":"Pen"}"#).unwrap();
        assert_eq!((partial.tool.as_str(), partial.styles.len()), ("Pen", 0));
    }

    #[test]
    fn shrinks_by_averaging() {
        // 4×2: a black and a white 2×2 block, halved to 2×1.
        let rgba: Vec<u8> = (0..8).flat_map(|i| if i % 4 < 2 { [0, 0, 0, 255] } else { [255, 255, 255, 255] }).collect();
        let (w, h, out) = shrink(4, 2, rgba.clone(), 0.5);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(shrink(4, 2, rgba.clone(), 1.0).2, rgba);
    }

    #[test]
    fn upscales() {
        // 2×1: black, white. Doubled.
        let rgba = vec![0, 0, 0, 255, 255, 255, 255, 255];
        let (w, h, px) = resize_image(2, 1, rgba.clone(), 2.0, Upscale::Pixels);
        assert_eq!((w, h), (4, 2));
        assert_eq!(px[..16], [0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255]);
        let (w, h, px) = resize_image(2, 1, rgba, 2.0, Upscale::Sharp);
        assert_eq!((w, h), (4, 2));
        assert!(px[0] < 40 && px[12] > 215, "dark stays dark, light stays light: {px:?}");
        // A flat image stays flat (the weights sum to 1, sharpening adds nothing).
        let flat = vec![100u8; 5 * 5 * 4];
        let (_, _, px) = resize_image(5, 5, flat, 3.0, Upscale::Sharp);
        assert!(px.iter().all(|&v| v == 100));
    }

    #[test]
    fn reduces_to_a_palette() {
        // Noisy, photo-like pixels: thousands of colors that lossless PNG can't squeeze.
        let mut seed = 12345u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        };
        let many: Vec<u8> = (0..64 * 64).flat_map(|_| [next(), next(), next(), 255]).collect();
        let reduced = reduce_colors(&many);
        let colors: std::collections::HashSet<_> = reduced.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect();
        assert!(colors.len() <= 256, "{} colors", colors.len());
        let png = encode_output(64, 64, &many, false, Output { reduce_colors: true, ..Default::default() }).unwrap();
        let full = encode_output(64, 64, &many, false, Output::default()).unwrap();
        assert!(png.len() < full.len(), "{} vs {}", png.len(), full.len());
    }

    /// Sizes and times for a real screenshot: `CLIPIKON_TEST_PNG=<file> cargo test --release -- --ignored --nocapture output_sizes`.
    #[test]
    #[ignore]
    fn output_sizes_of_a_real_screenshot() {
        let file = std::env::var("CLIPIKON_TEST_PNG").expect("CLIPIKON_TEST_PNG");
        let img = crate::screenshot::decode_png(&std::fs::read(file).unwrap()).unwrap();
        let rgba: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        for (scale, reduce) in [(1.0, false), (1.0, true), (0.5, false), (0.5, true)] {
            let t = std::time::Instant::now();
            let (w, h, px) = resize_image(img.width, img.height, rgba.clone(), scale, Upscale::Sharp);
            let png = encode_output(w, h, &px, false, Output { scale, reduce_colors: reduce, jpeg_quality: 90, ..Default::default() }).unwrap();
            let png_t = t.elapsed();
            let jpg = encode_output(w, h, &px, true, Output { scale, reduce_colors: reduce, jpeg_quality: 90, ..Default::default() }).unwrap();
            println!("{w}x{h} reduce={reduce}: PNG {} in {png_t:?}, JPEG90 {}", kb(png.len()), kb(jpg.len()));
        }
    }

    #[test]
    fn crop_handles() {
        let r = Rect::from_min_max(pos2(10.0, 10.0), pos2(110.0, 60.0));
        assert_eq!(crop_hit(r, pos2(11.0, 9.0), 4.0), Some((-1, -1)));
        assert_eq!(crop_hit(r, pos2(60.0, 35.0), 4.0), Some((0, 0)));
        assert_eq!(crop_hit(r, pos2(200.0, 35.0), 4.0), None);
        assert_eq!(resize(r, 1, 0, pos2(5.0, 0.0)).width(), 4.0);
    }
}
