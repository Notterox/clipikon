//! Trim + crop + export window (egui, Fluent style). Runs as its own process: `clipikon edit <file>`.

use crate::export::{self, Codec, Dither, Format, GifOptions, Mode, Options, Quality};
use crate::ffmpeg::{self, MediaInfo};
use crate::fluent::{self, TEXT, TEXT2, TEXT3, icon};
use crate::preview::Preview;
use crate::settings::{EditorPrefs, Upscaler};
use crate::video_view::VideoView;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, UiBuilder, pos2, vec2,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run(file: PathBuf) -> Result<(), String> {
    let info = ffmpeg::probe(&file)?;
    let title = format!("Clipikon — {}", file.file_name().unwrap_or_default().to_string_lossy());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0])
            .with_icon(egui::IconData { rgba: crate::icon_art::render(64, crate::icon_art::Style::Color), width: 64, height: 64 })
            // For Mica; without it (Windows 10) the panels paint an opaque background.
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Clipikon", options, Box::new(move |cc| Ok(Box::new(Editor::new(cc, file, info)))))
        .map_err(|e| e.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    Start,
    End,
    Playhead,
}

#[derive(Clone, Copy, PartialEq)]
enum Ratio {
    Free,
    Original,
    Wide,
    Vertical,
    Square,
    Portrait,
}

impl Ratio {
    const ALL: [Ratio; 6] = [Ratio::Free, Ratio::Original, Ratio::Wide, Ratio::Vertical, Ratio::Square, Ratio::Portrait];

    fn label(self) -> &'static str {
        match self {
            Ratio::Free => "Free",
            Ratio::Original => "Original",
            Ratio::Wide => "16:9",
            Ratio::Vertical => "9:16",
            Ratio::Square => "1:1",
            Ratio::Portrait => "4:5",
        }
    }

    /// Width / height, or None for free.
    fn value(self, source_aspect: f32) -> Option<f32> {
        match self {
            Ratio::Free => None,
            Ratio::Original => Some(source_aspect),
            Ratio::Wide => Some(16.0 / 9.0),
            Ratio::Vertical => Some(9.0 / 16.0),
            Ratio::Square => Some(1.0),
            Ratio::Portrait => Some(4.0 / 5.0),
        }
    }
}

/// What a drag on the crop overlay changes: the whole rectangle, or edges (-1 = left/top, 1 = right/bottom).
#[derive(Clone, Copy, PartialEq)]
enum CropDrag {
    Move,
    Resize(i8, i8),
}

/// Crop mode state; the rectangle itself lives in `Editor::crop`.
struct CropEdit {
    /// Crop when the mode was entered, restored by Esc.
    before: Option<Rect>,
    drag: Option<(CropDrag, Rect, Pos2)>,
}

struct ExportJob {
    output: PathBuf,
    progress: Arc<Mutex<f64>>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    started: Instant,
}

/// Filmstrip for the timeline: evenly spaced keyframes, decoded once in the background.
struct Thumbs {
    width: u32,
    height: u32,
    frames: Vec<Vec<u8>>,
}

/// A GIF rendered by the real encoder and played in the preview area, so palette, dithering, speed and frame
/// rate look exactly as exported. Export copies its file when the settings haven't changed since.
struct GifPreview {
    /// `Editor::gif_key()` it was rendered from.
    key: String,
    file: PathBuf,
    width: u32,
    height: u32,
    fps: f64,
    loop_forever: bool,
    expected_frames: usize,
    /// Per frame: width×height palette indices, then a 1024-byte BGRA palette.
    frames: Arc<Mutex<Vec<Vec<u8>>>>,
    done: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    pid: u32,
    texture: Option<egui::TextureHandle>,
    shown_frame: usize,
    playing_since: Option<Instant>,
}

impl GifPreview {
    fn ok(&self) -> bool {
        self.done.load(Ordering::Relaxed) && self.error.lock().unwrap().is_none()
    }

    fn size(&self) -> Option<u64> {
        self.ok().then(|| std::fs::metadata(&self.file).map(|m| m.len()).ok()).flatten()
    }
}

impl Drop for GifPreview {
    fn drop(&mut self) {
        if !self.done.load(Ordering::Relaxed) {
            ffmpeg::kill_pid(self.pid);
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Frames beyond this are encoded into the GIF but not kept for playback.
const GIF_PREVIEW_MAX_BYTES: usize = 400 << 20;

const THUMB_COUNT: usize = 12;
const VIDEO_FORMATS: [Format; 3] = [Format::Mp4, Format::Mkv, Format::WebM];
const MIN_CROP: f32 = 32.0;

struct Editor {
    file: PathBuf,
    info: MediaInfo,
    preview: Preview,
    /// Draws the frame (with upscaling); `texture` is the fallback if its shaders don't work here.
    view: VideoView,
    texture: Option<egui::TextureHandle>,
    upscaler: Upscaler,
    /// A new decode size for the preview, applied once the view has kept it for a moment.
    pending_size: Option<((u32, u32), Instant)>,
    thumbs: Arc<Mutex<Option<Thumbs>>>,
    thumb_textures: Vec<egui::TextureHandle>,
    accent: Color32,
    panel_fill: Color32,
    start: f64,
    end: f64,
    position: f64,
    playing: bool,
    drag: Drag,
    /// Source-pixel rectangle to keep; None = whole frame.
    crop: Option<Rect>,
    ratio: Ratio,
    cropping: Option<CropEdit>,
    // export options
    format: Format,
    /// Last video format picked, restored when switching back from GIF.
    video_format: Format,
    mode: Mode,
    codec: Codec,
    quality: Quality,
    size_text: String,
    max_height: i32,
    fps: u32,
    audio: bool,
    gif: GifOptions,
    job: Option<ExportJob>,
    status: String,
    last_export: Option<PathBuf>,
    /// Result message of a "Copy frame" running in the background.
    frame_copy: Arc<Mutex<Option<String>>>,
    gif_preview: Option<GifPreview>,
    /// Whether the preview area shows the GIF preview (rather than the video).
    gif_preview_visible: bool,
    gif_preview_count: u32,
    ctx: egui::Context,
}

impl Editor {
    fn new(cc: &eframe::CreationContext<'_>, file: PathBuf, info: MediaInfo) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let mica = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => fluent::style_window(h.hwnd.get(), false),
            _ => false,
        };
        let accent = fluent::install(&cc.egui_ctx, mica);
        // Starts at 1280 wide; follows the view once it's laid out.
        let mut preview = Preview::new(cc.egui_ctx.clone(), file.clone(), info.width, info.height, info.fps, info.duration, 1280);
        preview.seek(0.0);
        Self {
            end: info.duration,
            audio: info.has_audio,
            thumbs: spawn_thumbs(&file, &info, cc.egui_ctx.clone()),
            thumb_textures: Vec::new(),
            accent,
            panel_fill: if mica { Color32::TRANSPARENT } else { fluent::BG },
            file,
            info,
            preview,
            view: VideoView::default(),
            texture: None,
            upscaler: EditorPrefs::load().upscaler,
            pending_size: None,
            start: 0.0,
            position: 0.0,
            playing: false,
            drag: Drag::None,
            crop: None,
            ratio: Ratio::Free,
            cropping: None,
            format: Format::Mp4,
            video_format: Format::Mp4,
            mode: Mode::TargetSize,
            codec: Codec::H264,
            quality: Quality::Balanced,
            size_text: "25".into(),
            max_height: -1,
            fps: 0,
            gif: GifOptions::default(),
            job: None,
            status: String::new(),
            last_export: None,
            frame_copy: Arc::default(),
            gif_preview: None,
            gif_preview_visible: false,
            gif_preview_count: 0,
            ctx: cc.egui_ctx.clone(),
        }
    }

    fn set_format(&mut self, f: Format) {
        self.format = f;
        if self.mode == Mode::FastCut && !f.allows_fast_cut() {
            self.mode = Mode::TargetSize;
        }
        if let Some(&first) = f.codecs().first() {
            if !f.codecs().contains(&self.codec) {
                self.codec = first;
            }
        }
    }

    /// Copies the frame under the playhead, at full resolution and cropped, to the clipboard as an image.
    fn copy_frame(&mut self) {
        let (file, t) = (self.file.clone(), self.position.min((self.info.duration - 1.0 / self.info.fps).max(0.0)));
        let [x, y, w, h] = self.crop_px().unwrap_or([0, 0, self.info.width, self.info.height]);
        let (out, ctx) = (self.frame_copy.clone(), self.ctx.clone());
        self.status = "Copying frame…".into();
        std::thread::spawn(move || {
            let args: Vec<String> = [
                "-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &format!("{t:.3}"), "-i", &file.to_string_lossy(),
                "-frames:v", "1", "-an", "-vf", &format!("crop={w}:{h}:{x}:{y}"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let result = (|| -> Result<(), String> {
                let mut child = ffmpeg::spawn(&args, false)?;
                let mut rgba = Vec::new();
                child.stdout.take().ok_or("no output")?.read_to_end(&mut rgba).map_err(|e| e.to_string())?;
                let _ = child.wait();
                crate::clipboard::copy_image(w, h, &rgba)
            })();
            *out.lock().unwrap() = Some(match result {
                Ok(()) => format!("Frame at {} copied ({w}×{h}). Paste it anywhere.", fmt_time(t)),
                Err(e) => format!("Couldn't copy the frame: {e}"),
            });
            ctx.request_repaint();
        });
    }

    fn min_range(&self) -> f64 {
        0.1f64.min(self.info.duration)
    }

    fn source_rect(&self) -> Rect {
        Rect::from_min_size(Pos2::ZERO, vec2(self.info.width as f32, self.info.height as f32))
    }

    fn seek(&mut self, t: f64) {
        self.playing = false;
        self.gif_preview_visible = false;
        self.position = t.clamp(0.0, self.info.duration);
        self.preview.seek(self.position);
    }

    /// Everything a GIF preview depends on; a preview made from another key is out of date.
    fn gif_key(&self) -> String {
        format!("{:?}|{:?}|{:.3}|{:.3}", self.gif, self.crop_px(), self.start, self.end)
    }

    fn current_gif_preview(&self) -> Option<&GifPreview> {
        let key = self.gif_key();
        self.gif_preview.as_ref().filter(|p| p.key == key)
    }

    /// Encodes the GIF once, into a temp file and, as raw frames, into the preview area.
    fn start_gif_preview(&mut self) {
        let Some(o) = self.options() else { return };
        self.preview.stop();
        self.playing = false;
        self.gif_preview = None; // stops and deletes the previous one
        self.gif_preview_count += 1;
        let dir = std::env::temp_dir().join("Clipikon");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!("preview-{}-{}.gif", std::process::id(), self.gif_preview_count));
        let (args, gg) = export::gif_preview(&self.file, &file, &self.info, self.start, self.end, &o);
        let mut child = match ffmpeg::spawn(&args, false) {
            Ok(c) => c,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let stderr = ffmpeg::collect_stderr(&mut child);
        let mut stdout = child.stdout.take().unwrap();
        let frame_len = (gg.width * gg.height) as usize + 1024;
        let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let (done, error): (Arc<AtomicBool>, Arc<Mutex<Option<String>>>) = (Arc::default(), Arc::default());
        let (f, d, e, ctx) = (frames.clone(), done.clone(), error.clone(), self.ctx.clone());
        let pid = child.id();
        std::thread::spawn(move || {
            let mut kept = 0;
            loop {
                let mut buf = vec![0u8; frame_len];
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                if kept + frame_len <= GIF_PREVIEW_MAX_BYTES {
                    kept += frame_len;
                    f.lock().unwrap().push(buf);
                }
                ctx.request_repaint();
            }
            if !child.wait().is_ok_and(|s| s.success()) {
                std::thread::sleep(std::time::Duration::from_millis(50)); // let stderr drain
                *e.lock().unwrap() = Some(ffmpeg::tail(&stderr.lock().unwrap(), 6));
            }
            d.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        self.gif_preview = Some(GifPreview {
            key: self.gif_key(),
            file,
            width: gg.width,
            height: gg.height,
            fps: gg.fps,
            loop_forever: self.gif.loop_forever,
            expected_frames: (((self.end - self.start) / gg.speed) * gg.fps).round().max(1.0) as usize,
            frames,
            done,
            error,
            pid,
            texture: None,
            shown_frame: usize::MAX,
            playing_since: None,
        });
        self.gif_preview_visible = true;
    }

    /// Draws the GIF preview into `rect`; false if there's none to show.
    fn draw_gif_preview(&mut self, ui: &egui::Ui, rect: Rect) -> bool {
        let key = self.gif_key();
        let Some(gp) = self.gif_preview.as_mut().filter(|p| p.key == key) else { return false };
        let painter = ui.painter_at(rect);
        let frames = gp.frames.lock().unwrap();
        let error = gp.error.lock().unwrap().clone();
        let center = |text: String, color: Color32| {
            painter.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), color);
        };
        if let Some(e) = error {
            center(format!("GIF preview failed:\n{e}"), TEXT2);
            return true;
        }
        if !gp.done.load(Ordering::Relaxed) || frames.is_empty() {
            let pct = (frames.len() * 100 / gp.expected_frames.max(1)).min(99);
            center(format!("Rendering GIF preview… {pct}%"), TEXT2);
            return true;
        }
        let since = *gp.playing_since.get_or_insert_with(Instant::now);
        let tick = (since.elapsed().as_secs_f64() * gp.fps) as usize;
        let i = if gp.loop_forever { tick % frames.len() } else { tick.min(frames.len() - 1) };
        if i != gp.shown_frame || gp.texture.is_none() {
            let (w, h) = (gp.width as usize, gp.height as usize);
            let (indices, palette) = frames[i].split_at(w * h);
            let pixels = indices.iter().map(|&k| {
                let p = &palette[k as usize * 4..];
                Color32::from_rgb(p[2], p[1], p[0])
            });
            let img = egui::ColorImage::new([w, h], pixels.collect());
            // Nearest: show the dithering pixels as they are.
            match &mut gp.texture {
                Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                None => gp.texture = Some(ui.ctx().load_texture("gif-preview", img, egui::TextureOptions::NEAREST)),
            }
            gp.shown_frame = i;
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(1.0 / gp.fps));
        let aspect = gp.width as f32 / gp.height as f32;
        let r = rect.shrink(1.0);
        let size = if r.width() / r.height() > aspect { vec2(r.height() * aspect, r.height()) } else { vec2(r.width(), r.width() / aspect) };
        let img_rect = Rect::from_center_size(r.center(), size);
        if let Some(t) = &gp.texture {
            painter.image(t.id(), img_rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
        }
        let mb = gp.file.metadata().map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0);
        let badge = format!("GIF preview · {}×{} · {mb:.1} MB · click to close", gp.width, gp.height);
        let galley = painter.layout_no_wrap(badge, FontId::proportional(12.0), TEXT);
        let at = rect.left_top() + vec2(12.0, 12.0);
        painter.rect_filled(Rect::from_min_size(at, galley.size() + vec2(16.0, 6.0)), 4, Color32::from_rgba_premultiplied(30, 30, 30, 235));
        painter.galley(at + vec2(8.0, 3.0), galley, TEXT);
        true
    }

    fn toggle_play(&mut self) {
        self.gif_preview_visible = false;
        if self.playing {
            self.preview.stop();
            self.playing = false;
            return;
        }
        // Preview plays the selection.
        if self.position < self.start || self.position >= self.end - 0.05 {
            self.position = self.start;
        }
        self.preview.play(self.position, self.end);
        self.playing = true;
    }

    fn set_in(&mut self) {
        self.start = self.position.min(self.end - self.min_range()).max(0.0);
    }

    fn set_out(&mut self) {
        self.end = self.position.max(self.start + self.min_range()).min(self.info.duration);
    }

    /// The crop in even source pixels, as ffmpeg's crop filter needs for 4:2:0 video.
    fn crop_px(&self) -> Option<[u32; 4]> {
        let r = self.crop?;
        let even = |v: f32| ((v / 2.0).round().max(0.0) as u32) * 2;
        let (x, y) = (even(r.left()).min(self.info.width - 2), even(r.top()).min(self.info.height - 2));
        let w = even(r.width()).clamp(2, (self.info.width - x) & !1);
        let h = even(r.height()).clamp(2, (self.info.height - y) & !1);
        Some([x, y, w, h])
    }

    fn options(&self) -> Option<Options> {
        let mb = self.size_text.trim().parse::<f64>().ok().filter(|v| *v > 0.0);
        if mb.is_none() && self.mode == Mode::TargetSize && self.format != Format::Gif {
            return None;
        }
        Some(Options {
            format: self.format,
            mode: self.mode,
            codec: self.codec,
            target_mb: mb.unwrap_or(25.0),
            quality: self.quality,
            max_height: self.max_height,
            fps: self.fps,
            audio: self.audio,
            crop: self.crop_px(),
            gif: self.gif,
        })
    }

    fn output_path(&self, o: &Options) -> PathBuf {
        let dir = self.file.parent().unwrap_or(Path::new("."));
        let stem = self.file.file_stem().unwrap_or_default().to_string_lossy();
        let name = match (o.format, o.mode) {
            (Format::Gif, _) => stem.to_string(),
            (_, Mode::FastCut) => format!("{stem}_trim"),
            (_, Mode::TargetSize) => format!("{stem}_{}MB", self.size_text.trim()),
            (_, Mode::Quality) => format!("{stem}_compressed"),
        };
        let ext = o.format.extension();
        let mut path = dir.join(format!("{name}.{ext}"));
        let mut i = 2;
        while path.exists() {
            path = dir.join(format!("{name} ({i}).{ext}"));
            i += 1;
        }
        path
    }

    fn start_export(&mut self) {
        let Some(o) = self.options() else { return };
        self.preview.stop();
        self.playing = false;
        let output = self.output_path(&o);
        // The preview already is this GIF.
        if o.format == Format::Gif {
            if let Some(gp) = self.current_gif_preview().filter(|p| p.ok()) {
                if std::fs::copy(&gp.file, &output).is_ok() {
                    let mb = gp.size().unwrap_or(0) as f64 / 1_048_576.0;
                    self.status = format!("Saved {} · {mb:.1} MB (from the preview)", output.file_name().unwrap_or_default().to_string_lossy());
                    self.last_export = Some(output);
                    return;
                }
            }
        }
        let plan = export::plan(&self.file, &output, &self.info, self.start, self.end, &o);
        let total = self.end - self.start;
        let job = ExportJob {
            output,
            progress: Arc::default(),
            cancel: Arc::default(),
            result: Arc::default(),
            started: Instant::now(),
        };
        let (progress, cancel, result) = (job.progress.clone(), job.cancel.clone(), job.result.clone());
        std::thread::spawn(move || {
            let r = ffmpeg::run(&plan.args, |t| *progress.lock().unwrap() = (t / total).min(1.0), &cancel);
            *result.lock().unwrap() = Some(r);
        });
        self.status = "Exporting…".into();
        self.last_export = None;
        self.job = Some(job);
    }

    fn poll_export(&mut self) {
        let Some(job) = &self.job else { return };
        let Some(result) = job.result.lock().unwrap().take() else { return };
        let job = self.job.take().unwrap();
        match result {
            Ok(()) => {
                let mb = std::fs::metadata(&job.output).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                self.status = format!(
                    "Saved {} · {mb:.1} MB · took {:.1} s",
                    job.output.file_name().unwrap_or_default().to_string_lossy(),
                    job.started.elapsed().as_secs_f64()
                );
                self.last_export = Some(job.output);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&job.output);
                self.status = if job.cancel.load(Ordering::Relaxed) { "Export cancelled.".into() } else { format!("Export failed:\n{e}") };
            }
        }
    }

    fn enter_crop(&mut self) {
        if self.cropping.is_none() {
            self.cropping = Some(CropEdit { before: self.crop, drag: None });
            self.crop.get_or_insert(self.source_rect());
        }
    }

    fn finish_crop(&mut self, keep: bool) {
        let Some(edit) = self.cropping.take() else { return };
        if !keep {
            self.crop = edit.before;
        }
        // A crop that keeps (almost) everything is no crop: keeps the lossless fast cut available.
        if self.crop.is_some_and(|r| r.width() >= self.info.width as f32 - 2.0 && r.height() >= self.info.height as f32 - 2.0) {
            self.crop = None;
        }
        if self.crop.is_some() && self.mode == Mode::FastCut {
            self.mode = Mode::TargetSize;
        }
    }

    /// Largest rectangle of the chosen ratio, centered where the crop is now.
    fn apply_ratio(&mut self, ratio: Ratio) {
        self.ratio = ratio;
        let bounds = self.source_rect();
        let Some(a) = ratio.value(bounds.aspect_ratio()) else { return };
        let center = self.crop.unwrap_or(bounds).center();
        let w = bounds.width().min(bounds.height() * a);
        let size = vec2(w, w / a);
        let mut r = Rect::from_center_size(center, size);
        r = r.translate(vec2((bounds.left() - r.left()).max(0.0) + (bounds.right() - r.right()).min(0.0), 0.0));
        r = r.translate(vec2(0.0, (bounds.top() - r.top()).max(0.0) + (bounds.bottom() - r.bottom()).min(0.0)));
        self.crop = Some(r);
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let frame = 1.0 / self.info.fps;
        let (copy, space, i, o, c, enter, esc, left, right, home, end, shift) = ctx.input(|inp| {
            (
                // egui turns Ctrl+C into a Copy event rather than a key press.
                inp.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                inp.key_pressed(Key::Space),
                inp.key_pressed(Key::I),
                inp.key_pressed(Key::O),
                inp.key_pressed(Key::C) && !inp.modifiers.command,
                inp.key_pressed(Key::Enter),
                inp.key_pressed(Key::Escape),
                inp.key_pressed(Key::ArrowLeft),
                inp.key_pressed(Key::ArrowRight),
                inp.key_pressed(Key::Home),
                inp.key_pressed(Key::End),
                inp.modifiers.shift,
            )
        });
        let step = if shift { 1.0 } else { frame };
        if copy {
            self.copy_frame();
        }
        if space {
            self.toggle_play();
        }
        if i {
            self.set_in();
        }
        if o {
            self.set_out();
        }
        if c {
            if self.cropping.is_some() { self.finish_crop(true) } else { self.enter_crop() }
        }
        if enter {
            self.finish_crop(true);
        }
        if esc {
            self.finish_crop(false);
        }
        if left {
            self.seek(self.position - step);
        }
        if right {
            self.seek(self.position + step);
        }
        if home {
            self.seek(self.start);
        }
        if end {
            self.seek(self.end);
        }
    }

    fn upload_thumbs(&mut self, ctx: &egui::Context) {
        if !self.thumb_textures.is_empty() {
            return;
        }
        let Some(t) = self.thumbs.lock().unwrap().take() else { return };
        self.thumb_textures = t
            .frames
            .iter()
            .enumerate()
            .map(|(i, rgba)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([t.width as usize, t.height as usize], rgba);
                ctx.load_texture(format!("thumb{i}"), img, egui::TextureOptions::LINEAR)
            })
            .collect();
    }

    // ---- preview ----

    fn video(&mut self, ui: &mut egui::Ui, height: f32) {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect(rect, CornerRadius::same(8), Color32::BLACK, Stroke::new(1.0, fluent::CARD_STROKE), StrokeKind::Inside);
        let src = self.source_rect();
        // While cropping, show the whole frame; otherwise just what will be exported.
        let shown = if self.cropping.is_some() { src } else { self.crop.unwrap_or(src) };
        let fit = |r: Rect, aspect: f32| {
            let r = r.shrink(1.0);
            let size = if r.width() / r.height() > aspect { vec2(r.height() * aspect, r.height()) } else { vec2(r.width(), r.width() / aspect) };
            Rect::from_center_size(r.center(), size)
        };
        if self.format == Format::Gif && self.gif_preview_visible && self.cropping.is_none() && self.draw_gif_preview(ui, rect) {
            if resp.clicked() {
                self.gif_preview_visible = false;
            }
            return;
        }
        let margin = if self.cropping.is_some() { 24.0 } else { 0.0 };
        let img = fit(rect.shrink(margin), shown.aspect_ratio());
        let uv = Rect::from_min_max(
            pos2(shown.left() / src.width(), shown.top() / src.height()),
            pos2(shown.right() / src.width(), shown.bottom() / src.height()),
        );
        if self.view.has_frame() && !self.view.failed() {
            self.view.paint(&painter, img, uv, self.upscaler);
        } else if let Some(tex) = &self.texture {
            painter.image(tex.id(), img, uv, Color32::WHITE);
        }
        self.follow_view_size(ui.ctx(), img, shown);
        if self.cropping.is_some() {
            self.crop_overlay(ui, &resp, img);
        } else if resp.clicked() {
            self.toggle_play();
        }
    }

    /// Decodes the preview at the size it's shown (`shown`, in source pixels, fills `img`), so a large view
    /// gets full detail and a small one doesn't decode pixels it can't show. Applied once the size has held
    /// for a moment, as it restarts the decoder.
    fn follow_view_size(&mut self, ctx: &egui::Context, img: Rect, shown: Rect) {
        let need = (img.width() * ctx.pixels_per_point() * self.info.width as f32 / shown.width()).ceil() as u32;
        let size = self.preview.size_for(need);
        if size == (self.preview.width, self.preview.height) {
            self.pending_size = None;
            return;
        }
        const SETTLE: Duration = Duration::from_millis(250);
        match self.pending_size {
            Some((s, since)) if s == size => {
                let held = since.elapsed();
                if held >= SETTLE {
                    self.pending_size = None;
                    self.preview.resize(size);
                    if self.playing {
                        self.preview.play(self.position, self.end);
                    } else {
                        self.preview.seek(self.position);
                    }
                } else {
                    ctx.request_repaint_after(SETTLE - held);
                }
            }
            _ => {
                self.pending_size = Some((size, Instant::now()));
                ctx.request_repaint_after(SETTLE);
            }
        }
    }

    fn crop_overlay(&mut self, ui: &egui::Ui, resp: &egui::Response, img: Rect) {
        let bounds = self.source_rect();
        let scale = img.width() / bounds.width();
        let to_screen = |p: Pos2| img.min + p.to_vec2() * scale;
        let to_source = |p: Pos2| ((p - img.min) / scale).to_pos2();
        let crop = self.crop.unwrap_or(bounds);
        let screen = Rect::from_min_max(to_screen(crop.min), to_screen(crop.max));

        let hit = |p: Pos2| -> Option<CropDrag> {
            let near = 12.0;
            let dx = if (p.x - screen.left()).abs() < near { -1 } else if (p.x - screen.right()).abs() < near { 1 } else { 0 };
            let dy = if (p.y - screen.top()).abs() < near { -1 } else if (p.y - screen.bottom()).abs() < near { 1 } else { 0 };
            let inside_band = screen.expand(near).contains(p);
            if inside_band && (dx != 0 || dy != 0) {
                Some(CropDrag::Resize(dx, dy))
            } else if screen.contains(p) {
                Some(CropDrag::Move)
            } else {
                None
            }
        };

        let ratio = self.ratio.value(bounds.aspect_ratio());
        if let Some(edit) = &mut self.cropping {
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    edit.drag = hit(p).map(|d| (d, crop, to_source(p)));
                }
            }
            if let (Some((kind, origin, grab)), Some(p)) = (edit.drag, resp.interact_pointer_pos()) {
                let p = to_source(p);
                self.crop = Some(match kind {
                    CropDrag::Move => {
                        let d = p - grab;
                        let d = vec2(
                            d.x.clamp(bounds.left() - origin.left(), bounds.right() - origin.right()),
                            d.y.clamp(bounds.top() - origin.top(), bounds.bottom() - origin.bottom()),
                        );
                        origin.translate(d)
                    }
                    CropDrag::Resize(sx, sy) => resize_crop(origin, sx, sy, p, bounds, ratio),
                });
            }
            if resp.drag_stopped() {
                edit.drag = None;
            }
        }
        let dragging = self.cropping.as_ref().and_then(|e| e.drag).map(|d| d.0);
        if let Some(kind) = dragging.or_else(|| resp.hover_pos().and_then(hit)) {
            ui.ctx().set_cursor_icon(match kind {
                CropDrag::Move => egui::CursorIcon::Move,
                CropDrag::Resize(x, y) if x == y => egui::CursorIcon::ResizeNwSe,
                CropDrag::Resize(0, _) => egui::CursorIcon::ResizeVertical,
                CropDrag::Resize(_, 0) => egui::CursorIcon::ResizeHorizontal,
                CropDrag::Resize(..) => egui::CursorIcon::ResizeNeSw,
            });
        }

        // Paint with the possibly updated rectangle.
        let crop = self.crop.unwrap_or(bounds);
        let s = Rect::from_min_max(to_screen(crop.min), to_screen(crop.max));
        let p = ui.painter_at(resp.rect);
        let dim = Color32::from_black_alpha(160);
        p.rect_filled(Rect::from_min_max(img.min, pos2(img.right(), s.top())), 0, dim);
        p.rect_filled(Rect::from_min_max(pos2(img.left(), s.bottom()), img.max), 0, dim);
        p.rect_filled(Rect::from_min_max(pos2(img.left(), s.top()), pos2(s.left(), s.bottom())), 0, dim);
        p.rect_filled(Rect::from_min_max(pos2(s.right(), s.top()), pos2(img.right(), s.bottom())), 0, dim);
        let thirds = Stroke::new(1.0, Color32::from_white_alpha(90));
        for k in [1.0 / 3.0, 2.0 / 3.0] {
            let x = s.left() + s.width() * k;
            let y = s.top() + s.height() * k;
            p.line_segment([pos2(x, s.top()), pos2(x, s.bottom())], thirds);
            p.line_segment([pos2(s.left(), y), pos2(s.right(), y)], thirds);
        }
        p.rect_stroke(s, 0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Middle);
        let (arm, w) = (18.0f32.min(s.width() / 3.0).min(s.height() / 3.0), 4.0);
        let bracket = Stroke::new(w, Color32::WHITE);
        for (c, sx, sy) in [(s.left_top(), 1.0, 1.0), (s.right_top(), -1.0, 1.0), (s.left_bottom(), 1.0, -1.0), (s.right_bottom(), -1.0, -1.0)] {
            let c = c + vec2(-sx * w / 2.0, -sy * w / 2.0);
            p.line_segment([c, c + vec2(sx * (arm + w / 2.0), 0.0)], bracket);
            p.line_segment([c, c + vec2(0.0, sy * (arm + w / 2.0))], bracket);
        }
        for (c, horizontal) in [(s.center_top(), true), (s.center_bottom(), true), (s.left_center(), false), (s.right_center(), false)] {
            let size = if horizontal { vec2(24.0, 4.0) } else { vec2(4.0, 24.0) };
            if (horizontal && s.width() > 72.0) || (!horizontal && s.height() > 72.0) {
                p.rect_filled(Rect::from_center_size(c, size), 2, Color32::WHITE);
            }
        }
        if let Some([_, _, w, h]) = self.crop_px() {
            let text = format!("{w} × {h}");
            let galley = p.layout_no_wrap(text, FontId::proportional(12.0), TEXT);
            let at = pos2(s.left(), (s.top() - galley.size().y - 10.0).max(img.top() + 4.0));
            let bg = Rect::from_min_size(at, galley.size() + vec2(16.0, 4.0));
            p.rect_filled(bg, 4, Color32::from_rgba_premultiplied(30, 30, 30, 235));
            p.galley(at + vec2(8.0, 2.0), galley, TEXT);
        }
    }

    // ---- transport + timeline ----

    fn transport(&mut self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
        // Narrow window: icon-only buttons so the side groups don't run into the centered playback controls.
        let compact = rect.width() < 860.0;
        let row = |r: Rect, layout: Layout| UiBuilder::new().max_rect(r).layout(layout);
        ui.scope_builder(row(rect, Layout::left_to_right(Align::Center)), |ui| {
            // Time over frame number, painted so the pair is centered on the row.
            let frame = (self.position * self.info.fps).round() as u64;
            let frames = (self.info.duration * self.info.fps).round() as u64;
            let p = ui.painter();
            let now = p.layout_no_wrap(fmt_time(self.position), FontId::proportional(14.0), TEXT);
            let total = p.layout_no_wrap(format!(" / {}", fmt_time(self.info.duration)), FontId::proportional(14.0), TEXT3);
            let frame_text = p.layout_no_wrap(format!("Frame {frame} / {frames}"), FontId::proportional(12.0), TEXT3);
            let (x, cy) = (rect.left(), rect.center().y);
            let width = (now.size().x + total.size().x).max(frame_text.size().x);
            p.galley(pos2(x, cy - now.size().y + 2.0), now.clone(), TEXT);
            p.galley(pos2(x + now.size().x, cy - total.size().y + 2.0), total, TEXT3);
            p.galley(pos2(x, cy + 2.0), frame_text, TEXT3);
            ui.add_space(width + 12.0);
            let tip = "Copy this frame as an image (Ctrl+C)";
            let copy = if compact {
                fluent::icon_button(ui, icon::IMAGE, tip)
            } else {
                fluent::button(ui, Some(icon::IMAGE), "Copy frame").on_hover_text(tip)
            };
            if copy.clicked() {
                self.copy_frame();
            }
            let selected = if compact { icon::ZOOM_IN.to_string() } else { format!("{}  {}", icon::ZOOM_IN, self.upscaler.label()) };
            let before = self.upscaler;
            fluent::combo("upscaler", selected)
                .width(if compact { 56.0 } else { 132.0 })
                .show_ui(ui, |ui| {
                    for (u, tip) in [
                        (Upscaler::Fsrcnnx, "Neural network (FSRCNNX): sharpest, a bit more GPU work"),
                        (Upscaler::Fsr, "AMD FSR 1: edge-aware upscale and sharpen"),
                        (Upscaler::Bilinear, "Plain bilinear filtering"),
                    ] {
                        ui.selectable_value(&mut self.upscaler, u, u.label()).on_hover_text(tip);
                    }
                })
                .response
                .on_hover_text("Upscaling, when the view is larger than the video");
            if self.upscaler != before {
                EditorPrefs { upscaler: self.upscaler }.save();
            }
        });
        let center = Rect::from_center_size(rect.center(), vec2(136.0, 40.0));
        ui.scope_builder(row(center, Layout::left_to_right(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if fluent::icon_button(ui, icon::PREVIOUS, "Previous frame (←)").clicked() {
                self.seek(self.position - 1.0 / self.info.fps);
            }
            let glyph = if self.playing { icon::PAUSE } else { icon::PLAY };
            if fluent::round_button(ui, glyph, 40.0).on_hover_text("Play/pause (Space)").clicked() {
                self.toggle_play();
            }
            if fluent::icon_button(ui, icon::NEXT, "Next frame (→)").clicked() {
                self.seek(self.position + 1.0 / self.info.fps);
            }
        });
        ui.scope_builder(row(rect, Layout::right_to_left(Align::Center)), |ui| {
            if fluent::button(ui, None, "Set out").on_hover_text("O").clicked() {
                self.set_out();
            }
            if fluent::button(ui, None, "Set in").on_hover_text("I").clicked() {
                self.set_in();
            }
            let label = match (self.crop_px(), self.cropping.is_some()) {
                (_, true) => "Cropping…".to_string(),
                (Some([_, _, w, h]), _) => format!("Crop · {w}×{h}"),
                (None, _) => "Crop".to_string(),
            };
            let active = self.crop.is_some() || self.cropping.is_some();
            let content = if compact { icon::CROP.to_string() } else { format!("{}  {label}", icon::CROP) };
            let text = if active { RichText::new(content).color(self.accent) } else { RichText::new(content) };
            let stroke = Stroke::new(1.0, if active { self.accent } else { fluent::CONTROL_STROKE });
            let tip = if compact { format!("{label} (C)") } else { "C".into() };
            if ui.add(egui::Button::new(text).stroke(stroke).min_size(vec2(32.0, 32.0))).on_hover_text(tip).clicked() {
                if self.cropping.is_some() { self.finish_crop(true) } else { self.enter_crop() }
            }
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::click_and_drag());
        let handle_w = 10.0;
        let track = Rect::from_min_max(pos2(rect.left() + handle_w, rect.top() + 6.0), pos2(rect.right() - handle_w, rect.bottom() - 2.0));
        let dur = self.info.duration.max(0.01);
        let x_of = |t: f64| track.left() + (t / dur) as f32 * track.width();
        let t_of = |x: f32| (((x - track.left()) / track.width()) as f64 * dur).clamp(0.0, dur);

        if let Some(p) = resp.interact_pointer_pos() {
            if resp.drag_started() || resp.clicked() || self.drag == Drag::None {
                let (xs, xe) = (x_of(self.start), x_of(self.end));
                self.drag = if p.x >= xs - handle_w - 3.0 && p.x <= xs + 3.0 {
                    Drag::Start
                } else if p.x >= xe - 3.0 && p.x <= xe + handle_w + 3.0 {
                    Drag::End
                } else {
                    Drag::Playhead
                };
            }
            // Dragging a handle also scrubs the preview to it, so the cut point can be seen.
            let t = t_of(p.x);
            match self.drag {
                Drag::Start => {
                    self.start = t.min(self.end - self.min_range()).max(0.0);
                    self.seek(self.start);
                }
                Drag::End => {
                    self.end = t.max(self.start + self.min_range()).min(dur);
                    self.seek(self.end);
                }
                Drag::Playhead => self.seek(t),
                Drag::None => {}
            }
        }
        if !resp.is_pointer_button_down_on() {
            self.drag = Drag::None;
        }
        if let Some(hover) = resp.hover_pos() {
            let (xs, xe) = (x_of(self.start), x_of(self.end));
            if (hover.x - xs).abs() < handle_w + 3.0 || (hover.x - xe).abs() < handle_w + 3.0 {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }

        let painter = ui.painter_at(rect.expand(2.0));
        // Filmstrip, each tile cropped to fill ("cover").
        if self.thumb_textures.is_empty() {
            painter.rect_filled(track, 4, fluent::CONTROL);
        } else {
            let n = self.thumb_textures.len();
            let tile_w = track.width() / n as f32;
            for (i, tex) in self.thumb_textures.iter().enumerate() {
                let tile = Rect::from_min_size(pos2(track.left() + tile_w * i as f32, track.top()), vec2(tile_w, track.height()));
                let [tw, th] = tex.size();
                let (tex_aspect, tile_aspect) = (tw as f32 / th as f32, tile.aspect_ratio());
                let uv = if tex_aspect > tile_aspect {
                    let k = tile_aspect / tex_aspect;
                    Rect::from_min_max(pos2(0.5 - k / 2.0, 0.0), pos2(0.5 + k / 2.0, 1.0))
                } else {
                    let k = tex_aspect / tile_aspect;
                    Rect::from_min_max(pos2(0.0, 0.5 - k / 2.0), pos2(1.0, 0.5 + k / 2.0))
                };
                painter.image(tex.id(), tile.shrink2(vec2(1.0, 0.0)), uv, Color32::WHITE);
            }
        }
        let (xs, xe) = (x_of(self.start), x_of(self.end));
        let dim = Color32::from_black_alpha(158);
        painter.rect_filled(Rect::from_min_max(track.min, pos2(xs, track.bottom())), CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, dim);
        painter.rect_filled(Rect::from_min_max(pos2(xe, track.top()), track.max), CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, dim);
        let sel = Rect::from_min_max(pos2(xs, track.top() - 2.0), pos2(xe, track.bottom() + 2.0));
        painter.line_segment([sel.left_top() + vec2(0.0, 1.0), sel.right_top() + vec2(0.0, 1.0)], Stroke::new(2.0, accent));
        painter.line_segment([sel.left_bottom() - vec2(0.0, 1.0), sel.right_bottom() - vec2(0.0, 1.0)], Stroke::new(2.0, accent));
        for (x0, r) in [(xs - handle_w, CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }), (xe, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 })] {
            let h = Rect::from_min_max(pos2(x0, sel.top()), pos2(x0 + handle_w, sel.bottom()));
            painter.rect_filled(h, r, accent);
            painter.rect_filled(Rect::from_center_size(h.center(), vec2(2.0, 16.0)), 1, Color32::from_black_alpha(140));
        }
        let xp = x_of(self.position);
        painter.line_segment([pos2(xp, rect.top() - 2.0), pos2(xp, rect.bottom())], Stroke::new(2.0, Color32::WHITE));
        painter.circle_filled(pos2(xp, rect.top()), 6.0, Color32::WHITE);

        // Time ruler under the strip.
        let (ruler, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
        let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0]
            .into_iter()
            .find(|s| dur / s <= (track.width() / 70.0) as f64)
            .unwrap_or(1200.0);
        let rp = ui.painter_at(ruler.expand2(vec2(20.0, 0.0)));
        let mut t = 0.0;
        while t <= dur + 1e-6 {
            let anchor = if t == 0.0 { Align2::LEFT_TOP } else if t + step > dur + 1e-6 && x_of(t) > track.right() - 30.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            rp.text(pos2(x_of(t), ruler.top()), anchor, fmt_short(t), FontId::proportional(12.0), TEXT3);
            t += step;
        }
    }

    fn timeline_card(&mut self, ui: &mut egui::Ui) {
        fluent::card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.add_space(2.0);
            self.timeline(ui);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 20.0;
                for (k, v) in [("In", self.start), ("Out", self.end), ("Selection", self.end - self.start)] {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(k).small().color(TEXT2));
                        ui.label(RichText::new(fmt_time(v)).font(fluent::semibold(12.0)).color(TEXT));
                    });
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("Space play · ←/→ frame · Shift+←/→ 1 s · I/O in/out · C crop").small().color(TEXT3));
                });
            });
        });
    }

    // ---- side panels ----

    /// Pins `add` to the bottom of the remaining space, using its height measured on the previous pass.
    fn footer(&mut self, ui: &mut egui::Ui, add: impl FnOnce(&mut Self, &mut egui::Ui)) {
        let id = ui.make_persistent_id("footer_height");
        let stored = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(0.0);
        let space = ui.available_height() - stored;
        if space > 0.0 {
            ui.add_space(space);
        }
        let top = ui.cursor().top();
        add(self, ui);
        let h = ui.min_rect().bottom() - top;
        if (h - stored).abs() > 0.5 {
            ui.ctx().data_mut(|d| d.insert_temp(id, h));
            ui.ctx().request_discard("footer height changed");
        }
    }

    fn export_panel(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        ui.add_space(4.0);
        ui.heading("Export");
        let size_mb = std::fs::metadata(&self.file).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
        ui.label(
            RichText::new(format!(
                "{}×{} · {:.0} fps · {} · {size_mb:.1} MB{}",
                self.info.width,
                self.info.height,
                self.info.fps,
                fmt_time(self.info.duration),
                if self.info.has_audio { "" } else { " · no audio" }
            ))
            .small()
            .color(TEXT2),
        );
        ui.add_space(6.0);
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            let gif = self.format == Format::Gif;
            match fluent::segmented(ui, accent, &["Video", "GIF"], gif as usize) {
                Some(0) => self.set_format(self.video_format),
                Some(_) => self.set_format(Format::Gif),
                None => {}
            }
            ui.add_space(4.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            if self.format == Format::Gif {
                self.gif_settings(ui);
            } else {
                self.video_settings(ui);
            }
        });

        self.export_footer(ui);
    }

    fn gif_settings(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        let g = &mut self.gif;
        fluent::card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            // Chips in a right-to-left row: listed last-first so they read left to right.
            fluent::setting_row(ui, "Frame rate", |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for f in [30, 20, 15, 10] {
                    if fluent::chip(ui, accent, &f.to_string(), g.fps == f).clicked() {
                        g.fps = f;
                    }
                }
            });
            fluent::setting_row(ui, "Speed", |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for s in [2.0, 1.5, 1.0, 0.5] {
                    if fluent::chip(ui, accent, &format!("{s}×"), g.speed == s).clicked() {
                        g.speed = s;
                    }
                }
            });
            fluent::setting_row(ui, "Width", |ui| {
                let label = |w: u32| if w == 0 { "Original".to_string() } else { format!("{w} px") };
                fluent::combo("gif_width", label(g.width)).show_ui(ui, |ui| {
                    for w in [320, 480, 640, 800, 0] {
                        ui.selectable_value(&mut g.width, w, label(w));
                    }
                });
            });
            fluent::setting_row(ui, "Colors", |ui| {
                let label = |c: u32| match c {
                    256 => "256 · best".to_string(),
                    32 => "32 · smallest".to_string(),
                    c => c.to_string(),
                };
                fluent::combo("gif_colors", label(g.colors)).show_ui(ui, |ui| {
                    for c in [256, 128, 64, 32] {
                        ui.selectable_value(&mut g.colors, c, label(c));
                    }
                });
            });
            fluent::setting_row(ui, "Dithering", |ui| {
                let label = |d: Dither| match d {
                    Dither::Pattern => "Pattern · stable",
                    Dither::Smooth => "Smooth · bigger",
                    Dither::None => "None · flat",
                };
                fluent::combo("gif_dither", label(g.dither)).show_ui(ui, |ui| {
                    for d in [Dither::Pattern, Dither::Smooth, Dither::None] {
                        ui.selectable_value(&mut g.dither, d, label(d));
                    }
                });
            });
            fluent::setting_row(ui, "Loop forever", |ui| {
                fluent::toggle(ui, accent, &mut g.loop_forever);
            });
            fluent::setting_row(ui, "Palette per frame", |ui| {
                fluent::toggle(ui, accent, &mut g.palette_per_frame);
            })
            .on_hover_text("Better colors when the scene changes a lot; bigger file");
            ui.add_space(4.0);
            ui.label(RichText::new("No sound. For anything longer than a few seconds, MP4 or WebM is far smaller.").small().color(TEXT2));
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let current = self.current_gif_preview();
            let rendering = current.is_some_and(|p| !p.done.load(Ordering::Relaxed));
            let (label, note) = match current {
                Some(_) if rendering => ("Rendering…", String::new()),
                Some(p) if p.ok() && self.gif_preview_visible => ("Hide preview", format!("{:.1} MB, exact", p.size().unwrap_or(0) as f64 / 1_048_576.0)),
                Some(p) if p.ok() => ("Show preview", format!("{:.1} MB, exact", p.size().unwrap_or(0) as f64 / 1_048_576.0)),
                _ => ("Preview GIF", "Shows the real look and exact size".into()),
            };
            let resp = ui.add_enabled(!rendering, egui::Button::new(format!("{}  {label}", icon::PLAY)).min_size(vec2(0.0, 32.0)));
            if resp.clicked() {
                match self.current_gif_preview().map(|p| p.ok()) {
                    Some(true) => self.gif_preview_visible = !self.gif_preview_visible,
                    _ => self.start_gif_preview(),
                }
            }
            ui.label(RichText::new(note).small().color(TEXT2));
        });
    }

    fn video_settings(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        {
            let cropped = self.crop.is_some();
            let copyable = self.format.allows_fast_cut();
            let sub = if !copyable {
                format!("Not available for {}", self.format.label())
            } else if cropped {
                "Not available with a crop".into()
            } else {
                "Instant, lossless; starts on a keyframe".into()
            };
            if fluent::radio_card(ui, accent, self.mode == Mode::FastCut, copyable && !cropped, "Original quality", &sub, |_| {}) {
                self.mode = Mode::FastCut;
            }
            let encoder = if self.codec == Codec::Av1 { "SVT-AV1 on the CPU" } else { "Two-pass NVENC" };
            let size_text = &mut self.size_text;
            if fluent::radio_card(ui, accent, self.mode == Mode::TargetSize, true, "Fit to file size", &format!("{encoder}, stays under the limit"), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add(egui::TextEdit::singleline(size_text).desired_width(40.0).margin(Margin::symmetric(8, 6)));
                    ui.label(RichText::new("MB").color(TEXT2));
                    for preset in ["10", "25", "50", "100"] {
                        if fluent::chip(ui, accent, preset, size_text.trim() == preset).clicked() {
                            *size_text = preset.into();
                        }
                    }
                });
            }) {
                self.mode = Mode::TargetSize;
            }
            let quality = &mut self.quality;
            if fluent::radio_card(ui, accent, self.mode == Mode::Quality, true, "Constant quality", "File size depends on the content", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (q, label) in [(Quality::High, "High"), (Quality::Balanced, "Balanced"), (Quality::Small, "Small")] {
                        if fluent::chip(ui, accent, label, *quality == q).clicked() {
                            *quality = q;
                        }
                    }
                });
            }) {
                self.mode = Mode::Quality;
            }

            ui.add_space(4.0);
            fluent::card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                // Enabled even for a lossless cut, which can go to MP4 or MKV.
                let mut format = self.format;
                fluent::setting_row(ui, "Format", |ui| {
                    let label = |f: Format| match f {
                        Format::WebM => "WebM · AV1 + Opus",
                        f => f.label(),
                    };
                    fluent::combo("format", label(format)).show_ui(ui, |ui| {
                        for f in VIDEO_FORMATS {
                            ui.selectable_value(&mut format, f, label(f));
                        }
                    });
                });
                if format != self.format {
                    self.video_format = format;
                    self.set_format(format);
                }
                ui.add_enabled_ui(self.mode != Mode::FastCut, |ui| {
                    fluent::setting_row(ui, "Codec", |ui| {
                        let label = |c: Codec| match c {
                            Codec::H264 => "H.264 · plays anywhere",
                            Codec::Hevc => "HEVC · ~40% smaller",
                            Codec::Av1 => "AV1 · smallest, CPU",
                        };
                        let choices = self.format.codecs();
                        ui.add_enabled_ui(choices.len() > 1, |ui| {
                            fluent::combo("codec", label(self.codec)).show_ui(ui, |ui| {
                                for &c in choices {
                                    ui.selectable_value(&mut self.codec, c, label(c));
                                }
                            });
                        });
                    });
                    fluent::setting_row(ui, "Resolution", |ui| {
                        let res_label = |h: i32| match h {
                            -1 => "Auto".to_string(),
                            0 => "Original".to_string(),
                            h => format!("{h}p"),
                        };
                        fluent::combo("res", res_label(self.max_height)).show_ui(ui, |ui| {
                            for h in [-1, 0, 1440, 1080, 720, 480] {
                                ui.selectable_value(&mut self.max_height, h, res_label(h));
                            }
                        });
                    });
                    fluent::setting_row(ui, "Frame rate", |ui| {
                        let fps_label = |f: u32| if f == 0 { "Original".to_string() } else { format!("{f} fps") };
                        fluent::combo("fps", fps_label(self.fps)).show_ui(ui, |ui| {
                            for f in [0, 60, 30] {
                                ui.selectable_value(&mut self.fps, f, fps_label(f));
                            }
                        });
                    });
                });
                let has_audio = self.info.has_audio;
                let audio = &mut self.audio;
                fluent::setting_row(ui, "Include audio", |ui| {
                    ui.add_enabled_ui(has_audio, |ui| fluent::toggle(ui, accent, audio));
                });
            });
        }
    }

    fn export_footer(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        self.footer(ui, |this, ui| {
            fluent::card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let summary = match this.options() {
                    Some(o) => export::plan(&this.file, Path::new("out.mp4"), &this.info, this.start, this.end, &o).summary,
                    None => "Enter a target size in MB.".into(),
                };
                ui.label(RichText::new("Estimated output").small().color(TEXT2));
                ui.label(RichText::new(summary).small().color(TEXT));
                if this.format == Format::Gif {
                    if let Some(bytes) = this.current_gif_preview().and_then(|p| p.size()) {
                        let mb = bytes as f64 / 1_048_576.0;
                        ui.label(RichText::new(format!("Previewed: {mb:.1} MB exactly. Export saves that file instantly.")).small().color(TEXT));
                    }
                }
                if let Some(job) = &this.job {
                    let p = *job.progress.lock().unwrap() as f32;
                    ui.add(egui::ProgressBar::new(p).desired_height(4.0).fill(accent).corner_radius(2));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
                let width = ui.available_width();
                if let Some(job) = &this.job {
                    if fluent::secondary_button(ui, "Cancel", vec2(width, 36.0)).clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                } else if fluent::accent_button(ui, accent, "Export", vec2(width, 36.0), this.options().is_some()).clicked() {
                    this.start_export();
                }
                let done = this.last_export.clone();
                ui.add_enabled_ui(done.is_some(), |ui| {
                    fluent::button_pair(ui, 32.0, |ui, size| {
                        let copy = fluent::secondary_button(ui, &format!("{}  Copy file", icon::COPY), size);
                        if copy.on_hover_text("Paste into Discord, Telegram, Explorer…").clicked() {
                            if let Some(path) = &done {
                                this.status = match crate::clipboard::copy_file(path) {
                                    Ok(()) => "File copied. Paste it into a chat or folder.".into(),
                                    Err(e) => format!("Copy failed: {e}"),
                                };
                            }
                        }
                        if fluent::secondary_button(ui, &format!("{}  Show in folder", icon::FOLDER), size).clicked() {
                            if let Some(path) = &done {
                                let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
                            }
                        }
                    });
                });
                if !this.status.is_empty() {
                    ui.label(RichText::new(&this.status).small().color(TEXT2));
                }
            });
        });
    }

    fn crop_panel(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        let bounds = self.source_rect();
        ui.add_space(4.0);
        ui.heading("Crop");
        ui.label(RichText::new("Applies to the whole clip · exports re-encode").small().color(TEXT2));
        ui.add_space(6.0);

        fluent::card(ui, |ui| {
            ui.label(RichText::new("Aspect ratio").color(TEXT2));
            let gap = 6.0;
            let tile_w = (ui.available_width() - gap * 2.0) / 3.0;
            for row in Ratio::ALL.chunks(3) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for &r in row {
                        if ratio_tile(ui, accent, r, self.ratio == r, bounds.aspect_ratio(), tile_w).clicked() {
                            self.apply_ratio(r);
                        }
                    }
                });
            }
        });
        ui.add_space(4.0);

        fluent::card(ui, |ui| {
            let mut c = self.crop.unwrap_or(bounds);
            let ratio = self.ratio.value(bounds.aspect_ratio());
            let (mut x, mut y, mut w, mut h) = (c.left().round(), c.top().round(), c.width().round(), c.height().round());
            let mut changed = None;
            // Two columns of header-over-field (WinUI NumberBox style), sized to the card.
            let gap = 12.0;
            let cell_w = ((ui.available_width() - gap) / 2.0).floor();
            let mut field = |ui: &mut egui::Ui, label: &str, v: &mut f32, max: f32, which: u8| {
                ui.vertical(|ui| {
                    ui.set_width(cell_w);
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.add(egui::Label::new(RichText::new(label).small().color(TEXT2)).extend());
                    let r = ui.add_sized(vec2(cell_w, 32.0), egui::DragValue::new(v).range(0.0..=max).speed(2.0).suffix(" px"));
                    if r.changed() {
                        changed = Some(which);
                    }
                });
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                field(ui, "X", &mut x, bounds.width() - MIN_CROP, 0);
                field(ui, "Y", &mut y, bounds.height() - MIN_CROP, 1);
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                field(ui, "Width", &mut w, bounds.width(), 2);
                field(ui, "Height", &mut h, bounds.height(), 3);
            });
            if let Some(which) = changed {
                match (which, ratio) {
                    (2, Some(a)) => h = w / a,
                    (3, Some(a)) => w = h * a,
                    _ => {}
                }
                w = w.clamp(MIN_CROP, bounds.width());
                h = h.clamp(MIN_CROP, bounds.height());
                if let Some(a) = ratio {
                    // Re-fit both if clamping broke the ratio.
                    if w / h > a { w = h * a } else { h = w / a }
                }
                x = x.clamp(0.0, bounds.width() - w);
                y = y.clamp(0.0, bounds.height() - h);
                c = Rect::from_min_size(pos2(x, y), vec2(w, h));
                self.crop = Some(c);
            }
        });
        ui.add_space(4.0);

        if let Some([_, _, w, h]) = self.crop_px() {
            fluent::card(ui, |ui| {
                fluent::setting_row(ui, "Output", |ui| {
                    ui.label(RichText::new(format!("{w} × {h}")).color(TEXT));
                });
                ui.label(RichText::new("The Export resolution setting still applies after cropping.").small().color(TEXT2));
            });
        }

        self.footer(ui, |this, ui| {
            fluent::button_pair(ui, 36.0, |ui, size| {
                if fluent::secondary_button(ui, "Reset", size).on_hover_text("Whole frame").clicked() {
                    this.ratio = Ratio::Free;
                    this.crop = Some(this.source_rect());
                }
                if fluent::accent_button(ui, accent, "Done", size, true).on_hover_text("Enter").clicked() {
                    this.finish_crop(true);
                }
            });
        });
    }
}

/// Resizes `origin` by dragging edge(s) `sx`/`sy` to source point `p`, keeping the opposite edge fixed (or the
/// center, on an axis not being dragged), the ratio if one is set, and the rectangle inside `bounds`.
fn resize_crop(origin: Rect, sx: i8, sy: i8, p: Pos2, bounds: Rect, ratio: Option<f32>) -> Rect {
    let anchor = |s: i8, lo: f32, hi: f32| match s {
        -1 => hi,
        1 => lo,
        _ => (lo + hi) / 2.0,
    };
    let (ax, ay) = (anchor(sx, origin.left(), origin.right()), anchor(sy, origin.top(), origin.bottom()));
    let mut w = if sx != 0 { ((p.x - ax) * sx as f32).max(MIN_CROP) } else { origin.width() };
    let mut h = if sy != 0 { ((p.y - ay) * sy as f32).max(MIN_CROP) } else { origin.height() };
    let room = |a: f32, s: i8, lo: f32, hi: f32| match s {
        -1 => a - lo,
        1 => hi - a,
        _ => 2.0 * (a - lo).min(hi - a),
    };
    let (rw, rh) = (room(ax, sx, bounds.left(), bounds.right()), room(ay, sy, bounds.top(), bounds.bottom()));
    match ratio {
        Some(a) => {
            if sx != 0 && sy != 0 {
                if w / h > a { h = w / a } else { w = h * a }
            } else if sx != 0 {
                h = w / a;
            } else {
                w = h * a;
            }
            let k = (rw / w).min(rh / h).min(1.0);
            w *= k;
            h *= k;
        }
        None => {
            w = w.min(rw);
            h = h.min(rh);
        }
    }
    let start = |a: f32, s: i8, size: f32| match s {
        -1 => a - size,
        1 => a,
        _ => a - size / 2.0,
    };
    Rect::from_min_size(pos2(start(ax, sx, w), start(ay, sy, h)), vec2(w, h))
}

/// Aspect-ratio choice: an outline of the shape over its label.
fn ratio_tile(ui: &mut egui::Ui, accent: Color32, r: Ratio, selected: bool, source_aspect: f32, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 56.0), Sense::click());
    let fill = if resp.is_pointer_button_down_on() {
        fluent::CONTROL_PRESSED
    } else if resp.hovered() {
        fluent::CONTROL_HOVER
    } else {
        fluent::CONTROL
    };
    let color = if selected { accent } else { TEXT };
    let p = ui.painter();
    p.rect(rect, 4, fill, Stroke::new(1.0, if selected { accent } else { fluent::CONTROL_STROKE }), StrokeKind::Inside);
    let a = r.value(source_aspect).unwrap_or(1.4);
    let size = if a >= 1.0 { vec2(22.0, 22.0 / a) } else { vec2(20.0 * a, 20.0) };
    let shape = Rect::from_center_size(rect.center() - vec2(0.0, 9.0), size);
    let stroke = Stroke::new(1.5, color);
    if r == Ratio::Free {
        p.rect_stroke(shape, 2, Stroke::new(1.0, color), StrokeKind::Middle);
        for c in [shape.left_top(), shape.right_bottom()] {
            p.circle_filled(c, 2.0, color);
        }
    } else {
        p.rect_stroke(shape, 2, stroke, StrokeKind::Middle);
    }
    p.text(rect.center_bottom() - vec2(0.0, 9.0), Align2::CENTER_BOTTOM, r.label(), FontId::proportional(12.0), color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Decodes `THUMB_COUNT` keyframes (only keyframes: cheap even for long clips) in the background.
fn spawn_thumbs(file: &Path, info: &MediaInfo, ctx: egui::Context) -> Arc<Mutex<Option<Thumbs>>> {
    let out: Arc<Mutex<Option<Thumbs>>> = Arc::default();
    let (file, info, out2) = (file.to_path_buf(), info.clone(), out.clone());
    std::thread::spawn(move || {
        let height = 72u32;
        let width = ((info.width as f64 * height as f64 / info.height.max(1) as f64 / 2.0).round() as u32 * 2).max(2);
        let rate = THUMB_COUNT as f64 / info.duration.max(0.1);
        let args: Vec<String> = [
            "-hide_banner", "-loglevel", "error", "-nostdin", "-skip_frame", "nokey", "-i", &file.to_string_lossy(), "-an",
            "-vf", &format!("fps={rate:.6},scale={width}:{height}"), "-frames:v", &THUMB_COUNT.to_string(),
            "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let Ok(mut child) = ffmpeg::spawn(&args, false) else { return };
        let mut buf = Vec::new();
        let _ = child.stdout.take().map(|mut s| s.read_to_end(&mut buf));
        let _ = child.wait();
        let size = (width * height * 4) as usize;
        let frames: Vec<Vec<u8>> = buf.chunks_exact(size).map(<[u8]>::to_vec).collect();
        if !frames.is_empty() {
            *out2.lock().unwrap() = Some(Thumbs { width, height, frames });
            ctx.request_repaint();
        }
    });
    out
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0);
    format!("{}:{:05.2}", (s / 60.0) as u32, s % 60.0)
}

fn fmt_short(s: f64) -> String {
    if s.fract().abs() > 1e-6 {
        format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0)
    } else {
        format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32)
    }
}

impl eframe::App for Editor {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // Mica shows through; without Mica the panels paint the background
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_export();
        self.keyboard(&ctx);
        self.upload_thumbs(&ctx);
        if let Some(msg) = self.frame_copy.lock().unwrap().take() {
            self.status = msg;
        }

        if let Some(f) = self.preview.take_frame() {
            if self.playing {
                self.position = f.time;
            }
            if self.view.failed() {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                match &mut self.texture {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("frame", img, egui::TextureOptions::LINEAR)),
                }
            } else {
                self.view.set_frame(f.width, f.height, f.rgba);
            }
        } else if self.view.failed() && self.texture.is_none() && !self.playing {
            // The view gave up after taking the first frame: decode it again for the fallback.
            self.preview.seek(self.position);
            self.texture = Some(ctx.load_texture("frame", egui::ColorImage::filled([2, 2], Color32::BLACK), egui::TextureOptions::LINEAR));
        }
        if self.playing && self.preview.is_finished() {
            self.playing = false;
            self.position = self.end;
        }

        let fill = self.panel_fill;
        let frame = move |m: Margin| egui::Frame::new().fill(fill).inner_margin(m);
        egui::Panel::right("side")
            .exact_size(360.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(frame(Margin { left: 0, right: 16, top: 8, bottom: 16 }))
            .show(ui, |ui| {
                // Text wraps instead of widening the panel (labels in horizontal rows would otherwise extend,
                // pushing the content over the video).
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                // Separate ids, so each panel keeps its own footer height.
                if self.cropping.is_some() {
                    ui.push_id("crop", |ui| self.crop_panel(ui));
                } else {
                    ui.push_id("export", |ui| self.export_panel(ui));
                }
            });
        egui::CentralPanel::no_frame().frame(frame(Margin { left: 16, right: 16, top: 8, bottom: 16 })).show(ui, |ui| {
            let video_h = (ui.available_height() - 40.0 - 140.0 - 2.0 * ui.spacing().item_spacing.y).max(160.0);
            self.video(ui, video_h);
            self.transport(ui);
            self.timeline_card(ui);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 2560.0, y: 1440.0 } };

    #[test]
    fn corner_resize_keeps_ratio_and_bounds() {
        let origin = Rect::from_min_size(pos2(1000.0, 400.0), vec2(400.0, 400.0));
        let r = resize_crop(origin, 1, 1, pos2(3000.0, 1300.0), BOUNDS, Some(1.0));
        assert!((r.width() - r.height()).abs() < 0.01);
        assert_eq!(r.min, origin.min);
        assert!(r.max.x <= 2560.0 && r.max.y <= 1440.0);
    }

    #[test]
    fn edge_resize_free_moves_one_edge() {
        let origin = Rect::from_min_size(pos2(100.0, 100.0), vec2(500.0, 300.0));
        let r = resize_crop(origin, -1, 0, pos2(50.0, 9999.0), BOUNDS, None);
        assert_eq!((r.left(), r.right(), r.top(), r.bottom()), (50.0, 600.0, 100.0, 400.0));
    }
}
