//! Turns trim + crop + compression choices into an ffmpeg command line: NVENC for H.264/HEVC, SVT-AV1 (CPU)
//! for AV1, a two-pass palette for GIF.

use crate::ffmpeg::MediaInfo;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    FastCut,
    TargetSize,
    Quality,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    Mp4,
    Mkv,
    WebM,
    Gif,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Mp4 => "MP4",
            Format::Mkv => "MKV",
            Format::WebM => "WebM",
            Format::Gif => "GIF",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Mkv => "mkv",
            Format::WebM => "webm",
            Format::Gif => "gif",
        }
    }

    /// Whether the recording's H.264 can be stream-copied into it.
    pub fn allows_fast_cut(self) -> bool {
        matches!(self, Format::Mp4 | Format::Mkv)
    }

    /// Video codecs the container takes (none for GIF, which has its own encoder).
    pub fn codecs(self) -> &'static [Codec] {
        match self {
            Format::Mp4 | Format::Mkv => &[Codec::H264, Codec::Hevc, Codec::Av1],
            Format::WebM => &[Codec::Av1],
            Format::Gif => &[],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Codec {
    H264,
    Hevc,
    /// SVT-AV1 on the CPU: NVENC AV1 needs an RTX 40 or newer.
    Av1,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Quality {
    High,
    Balanced,
    Small,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub format: Format,
    pub mode: Mode,
    pub codec: Codec,
    pub target_mb: f64,
    pub quality: Quality,
    /// 0 = keep source; -1 = pick automatically from the bitrate budget (target-size mode).
    pub max_height: i32,
    /// 0 = keep source.
    pub fps: u32,
    pub audio: bool,
    /// Source-pixel rectangle `[x, y, w, h]` (even values) to keep. Needs a re-encode, so FastCut ignores it.
    pub crop: Option<[u32; 4]>,
    pub gif: GifOptions,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Dither {
    /// Ordered (Bayer): a fixed pattern that doesn't crawl between frames and compresses well.
    Pattern,
    /// Floyd–Steinberg error diffusion: smoothest gradients, noisier between frames, bigger files.
    Smooth,
    /// Flat color bands, smallest files.
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct GifOptions {
    pub fps: u32,
    /// 0 = keep source width.
    pub width: u32,
    /// Playback speed: 2.0 plays twice as fast (and halves the length).
    pub speed: f64,
    /// Palette size, 2–256.
    pub colors: u32,
    pub dither: Dither,
    pub loop_forever: bool,
    /// A palette per frame instead of one for the clip: better colors across scene changes, bigger files.
    pub palette_per_frame: bool,
}

impl Default for GifOptions {
    fn default() -> Self {
        Self { fps: 15, width: 480, speed: 1.0, colors: 256, dither: Dither::Pattern, loop_forever: true, palette_per_frame: false }
    }
}

pub struct Plan {
    pub args: Vec<String>,
    pub summary: String,
}

const TARGET_AUDIO_KBPS: u32 = 128;
const HEIGHTS: [u32; 7] = [2160, 1440, 1080, 720, 540, 480, 360];

pub fn plan(input: &Path, output: &Path, src: &MediaInfo, start: f64, end: f64, o: &Options) -> Plan {
    let dur = (end - start).max(0.01);
    let mut args: Vec<String> = Vec::new();
    let fast_cut = o.mode == Mode::FastCut && o.format.allows_fast_cut();

    if !fast_cut {
        push(&mut args, &["-hwaccel", "cuda"]); // GPU decode; ffmpeg falls back to CPU if unavailable
    }
    // -ss/-t on the input: they select source time, so a GIF speed change doesn't alter what's cut.
    push(&mut args, &["-ss", &format!("{start:.3}"), "-t", &format!("{dur:.3}"), "-i", &input.to_string_lossy(), "-map", "0:v:0"]);

    if o.format == Format::Gif {
        return gif(args, output, src, dur, o);
    }

    let with_audio = o.audio && src.has_audio;
    if with_audio {
        push(&mut args, &["-map", "0:a:0?"]);
    }
    let container: &[&str] = if o.format == Format::Mp4 { &["-movflags", "+faststart"] } else { &[] };

    if fast_cut {
        // No re-encode: instant and lossless, but the start snaps back to the previous keyframe (≤1 s).
        push(&mut args, &["-c", "copy", "-avoid_negative_ts", "make_zero"]);
        push(&mut args, container);
        push(&mut args, &[&output.to_string_lossy()]);
        return Plan {
            args,
            summary: "Copies the original stream: lossless and instant. The start snaps to the nearest keyframe (up to 1 s earlier).".into(),
        };
    }

    // WebM only takes AV1 here; the editor offers nothing else, but don't produce an invalid file regardless.
    let codec = if o.format.codecs().contains(&o.codec) { o.codec } else { Codec::Av1 };
    // From here on the cropped picture is "the source": resolution choices and bitrate math apply to it.
    let cropped;
    let src = match o.crop {
        Some([_, _, w, h]) => {
            cropped = MediaInfo { width: w, height: h, ..src.clone() };
            &cropped
        }
        None => src,
    };
    let fps = if o.fps > 0 { (o.fps as f64).min(src.fps) } else { src.fps };
    let audio_kbps = if with_audio { if o.mode == Mode::TargetSize { TARGET_AUDIO_KBPS } else { 160 } } else { 0 };
    let mut height = if o.max_height > 0 { (o.max_height as u32).min(src.height) } else { src.height };
    let mut rc: Vec<String> = Vec::new();
    let summary;

    if o.mode == Mode::TargetSize {
        // Decimal MB with 3% margin for container overhead, so "25 MB" fits both 25 MB and 25 MiB limits.
        let total_kbps = o.target_mb * 1_000_000.0 * 8.0 / 1000.0 * 0.97 / dur;
        let video_kbps = ((total_kbps - audio_kbps as f64) as u32).max(100);
        if o.max_height < 0 {
            height = auto_height(src, video_kbps, fps, codec);
        }
        match codec {
            Codec::Av1 => push(&mut rc, &["-b:v", &format!("{video_kbps}k")]),
            _ => push(&mut rc, &["-rc", "vbr", "-b:v", &format!("{video_kbps}k"), "-maxrate", &format!("{}k", video_kbps * 3 / 2), "-bufsize", &format!("{}k", video_kbps * 2)]),
        }
        let bpp = video_kbps as f64 * 1000.0 / (width_for(src, height) as f64 * height as f64 * fps);
        summary = format!(
            "{:.1} Mbps video at {height}p{fps:.0}{}",
            video_kbps as f64 / 1000.0,
            if bpp < min_bpp(codec) { " — low for this size; expect blockiness. Try a lower resolution/fps or another codec." } else { "" }
        );
    } else {
        let level = match o.quality {
            Quality::High => 0,
            Quality::Balanced => 1,
            Quality::Small => 2,
        };
        match codec {
            Codec::Av1 => {
                let crf = [28, 34, 40][level];
                push(&mut rc, &["-crf", &crf.to_string()]);
                summary = format!("Constant quality (CRF {crf}) at {height}p{fps:.0}; the size depends on the content.");
            }
            _ => {
                let cq = [21, 25, 29][level] + if codec == Codec::Hevc { 2 } else { 0 };
                push(&mut rc, &["-rc", "vbr", "-cq", &cq.to_string(), "-b:v", "0"]);
                summary = format!("Constant quality (CQ {cq}) at {height}p{fps:.0}; the size depends on the content.");
            }
        }
    }

    let mut filters = Vec::new();
    if let Some([x, y, w, h]) = o.crop {
        filters.push(format!("crop={w}:{h}:{x}:{y}"));
    }
    if height < src.height {
        filters.push(format!("scale=-2:{height}:flags=lanczos:reset_sar=1"));
    }
    if o.fps > 0 && (o.fps as f64) < src.fps - 0.5 {
        filters.push(format!("fps={}", o.fps));
    }
    if !filters.is_empty() {
        push(&mut args, &["-vf", &filters.join(",")]);
    }

    let gop = ((fps * 2.0).round() as u32).to_string();
    match codec {
        Codec::H264 => push(&mut args, &["-c:v", "h264_nvenc", "-profile:v", "high"]),
        Codec::Hevc => {
            push(&mut args, &["-c:v", "hevc_nvenc", "-profile:v", "main"]);
            if o.format == Format::Mp4 {
                push(&mut args, &["-tag:v", "hvc1"]); // what Apple players expect
            }
        }
        Codec::Av1 => push(&mut args, &["-c:v", "libsvtav1", "-preset", "8", "-pix_fmt", "yuv420p", "-g", &gop]),
    }
    if codec != Codec::Av1 {
        push(&mut args, &[
            "-preset", "p7", "-tune", "hq", "-multipass", "fullres", "-rc-lookahead", "32",
            "-spatial-aq", "1", "-temporal-aq", "1", "-bf", "3", "-b_ref_mode", "middle", "-pix_fmt", "yuv420p", "-g", &gop,
        ]);
    }
    args.extend(rc);
    if audio_kbps == 0 {
        push(&mut args, &["-an"]);
    } else if o.format == Format::WebM {
        push(&mut args, &["-c:a", "libopus", "-b:a", &format!("{audio_kbps}k"), "-ac", "2", "-ar", "48000"]);
    } else {
        push(&mut args, &["-c:a", "aac", "-b:a", &format!("{audio_kbps}k"), "-ac", "2"]);
    }
    push(&mut args, container);
    push(&mut args, &[&output.to_string_lossy()]);
    Plan { args, summary }
}

fn gif(mut args: Vec<String>, output: &Path, src: &MediaInfo, dur: f64, o: &Options) -> Plan {
    let gg = gif_graph(src, o);
    push(&mut args, &["-vf", &gg.graph, "-an", "-loop", gif_loop(o), &output.to_string_lossy()]);
    Plan { args, summary: gg.summary(dur) }
}

/// -loop: 0 = forever, -1 = play once.
fn gif_loop(o: &Options) -> &'static str {
    if o.gif.loop_forever { "0" } else { "-1" }
}

pub struct GifGraph {
    /// Filters from the cut's video to the paletted GIF frames.
    pub graph: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub speed: f64,
    pub colors: u32,
}

impl GifGraph {
    fn summary(&self, dur: f64) -> String {
        let speed = if (self.speed - 1.0).abs() > 1e-3 { format!(", {}× speed", self.speed) } else { String::new() };
        format!(
            "{}×{} at {:.0} fps, {} colors{speed}, {:.1} s long. GIFs get big fast: keep them short and small.",
            self.width, self.height, self.fps, self.colors, dur / self.speed
        )
    }
}

/// One ffmpeg run for the GIF preview: writes the GIF to `gif_out` and streams the same frames to stdout as
/// raw pal8 (per frame: width×height palette indices, then a 1024-byte BGRA palette).
pub fn gif_preview(input: &Path, gif_out: &Path, src: &MediaInfo, start: f64, end: f64, o: &Options) -> (Vec<String>, GifGraph) {
    let dur = (end - start).max(0.01);
    let gg = gif_graph(src, o);
    let mut args = Vec::new();
    push(&mut args, &[
        "-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-hwaccel", "cuda", "-ss", &format!("{start:.3}"), "-t", &format!("{dur:.3}"), "-i", &input.to_string_lossy(),
        "-filter_complex", &format!("[0:v:0]{},split[g][r]", gg.graph),
        "-map", "[g]", "-an", "-loop", gif_loop(o), &gif_out.to_string_lossy(),
        "-map", "[r]", "-f", "rawvideo", "-pix_fmt", "pal8", "pipe:1",
    ]);
    (args, gg)
}

/// GIF via palettegen/paletteuse in one graph. With one palette for the clip, palettegen weighs the pixels
/// that change (stats_mode=diff) and paletteuse only re-dithers changed rectangles, which keeps files small.
pub fn gif_graph(src: &MediaInfo, o: &Options) -> GifGraph {
    let g = &o.gif;
    let mut chain = Vec::new();
    let (mut w, mut h) = (src.width, src.height);
    if let Some([x, y, cw, ch]) = o.crop {
        chain.push(format!("crop={cw}:{ch}:{x}:{y}"));
        (w, h) = (cw, ch);
    }
    let speed = g.speed.clamp(0.1, 10.0);
    if (speed - 1.0).abs() > 1e-3 {
        chain.push(format!("setpts=PTS/{speed}"));
    }
    let fps = (g.fps.max(1) as f64).min(src.fps * speed);
    chain.push(format!("fps={fps}"));
    if g.width > 0 && g.width < w {
        h = ((h as f64 * g.width as f64 / w as f64).round() as u32).max(1);
        w = g.width;
        // Explicit height: the preview reads fixed-size frames, so it must match exactly.
        chain.push(format!("scale={w}:{h}:flags=lanczos"));
    }
    let colors = g.colors.clamp(2, 256);
    let (stats, new) = if g.palette_per_frame { ("single", ":new=1") } else { ("diff", "") };
    let dither = match g.dither {
        Dither::Pattern => "bayer:bayer_scale=4",
        Dither::Smooth => "floyd_steinberg",
        Dither::None => "none",
    };
    let graph = format!(
        "{},split[a][b];[a]palettegen=max_colors={colors}:stats_mode={stats}[p];[b][p]paletteuse=dither={dither}:diff_mode=rectangle{new}",
        chain.join(",")
    );
    GifGraph { graph, width: w, height: h, fps, speed, colors }
}

fn push(a: &mut Vec<String>, items: &[&str]) {
    a.extend(items.iter().map(|s| s.to_string()));
}

fn min_bpp(c: Codec) -> f64 {
    match c {
        Codec::H264 => 0.055,
        Codec::Hevc => 0.035,
        Codec::Av1 => 0.03,
    }
}

fn width_for(src: &MediaInfo, h: u32) -> u32 {
    ((src.width as f64 * h as f64 / src.height as f64 / 2.0).round() as u32) * 2
}

/// Highest standard height (≤ source) at which the bitrate still gives decent bits per pixel.
fn auto_height(src: &MediaInfo, video_kbps: u32, fps: f64, codec: Codec) -> u32 {
    std::iter::once(src.height)
        .chain(HEIGHTS.into_iter().filter(|&h| h < src.height))
        .find(|&h| video_kbps as f64 * 1000.0 / (width_for(src, h) as f64 * h as f64 * fps) >= min_bpp(codec))
        .unwrap_or(src.height.min(360))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> MediaInfo {
        MediaInfo { duration: 60.0, width: 2560, height: 1440, fps: 60.0, has_audio: true }
    }

    fn opts(mode: Mode) -> Options {
        Options {
            format: Format::Mp4,
            mode,
            codec: Codec::H264,
            target_mb: 25.0,
            quality: Quality::Balanced,
            max_height: -1,
            fps: 0,
            audio: true,
            crop: None,
            gif: GifOptions::default(),
        }
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str())
    }

    #[test]
    fn target_size_budget_fits() {
        let p = plan(Path::new("in.mp4"), Path::new("out.mp4"), &src(), 10.0, 40.0, &opts(Mode::TargetSize));
        let kbps: f64 = arg_after(&p.args, "-b:v").unwrap().trim_end_matches('k').parse().unwrap();
        let bytes = (kbps + TARGET_AUDIO_KBPS as f64) * 1000.0 / 8.0 * 30.0;
        assert!(bytes <= 25_000_000.0 && bytes > 23_000_000.0, "{bytes}");
        assert_eq!(arg_after(&p.args, "-ss"), Some("10.000"));
        assert_eq!(arg_after(&p.args, "-t"), Some("30.000"));
    }

    #[test]
    fn auto_resolution_downscales_small_budgets() {
        let mut o = opts(Mode::TargetSize);
        o.target_mb = 10.0;
        let p = plan(Path::new("in.mp4"), Path::new("out.mp4"), &src(), 0.0, 60.0, &o);
        let vf = arg_after(&p.args, "-vf").unwrap();
        assert!(vf.starts_with("scale=-2:"), "{vf}");
        let h: u32 = vf["scale=-2:".len()..].split(':').next().unwrap().parse().unwrap();
        assert!(h < 1440);
    }

    #[test]
    fn fast_cut_copies_streams() {
        let p = plan(Path::new("in.mp4"), Path::new("out.mp4"), &src(), 1.0, 2.0, &opts(Mode::FastCut));
        assert_eq!(arg_after(&p.args, "-c"), Some("copy"));
        assert!(!p.args.iter().any(|a| a.contains("nvenc")));
    }

    #[test]
    fn crop_then_scale() {
        let mut o = opts(Mode::Quality);
        o.crop = Some([874, 0, 810, 1440]);
        o.max_height = 1080;
        let p = plan(Path::new("in.mp4"), Path::new("out.mp4"), &src(), 0.0, 5.0, &o);
        assert_eq!(arg_after(&p.args, "-vf"), Some("crop=810:1440:874:0,scale=-2:1080:flags=lanczos:reset_sar=1"));
    }

    #[test]
    fn hevc_and_mute() {
        let mut o = opts(Mode::Quality);
        o.codec = Codec::Hevc;
        o.audio = false;
        let p = plan(Path::new("in.mp4"), Path::new("out.mp4"), &src(), 0.0, 5.0, &o);
        assert_eq!(arg_after(&p.args, "-c:v"), Some("hevc_nvenc"));
        assert!(p.args.contains(&"-an".to_string()));
        assert!(!p.args.contains(&"0:a:0?".to_string()));
    }

    #[test]
    fn webm_is_av1_and_opus_without_mp4_flags() {
        let mut o = opts(Mode::TargetSize);
        o.format = Format::WebM;
        let p = plan(Path::new("in.mp4"), Path::new("out.webm"), &src(), 0.0, 30.0, &o);
        assert_eq!(arg_after(&p.args, "-c:v"), Some("libsvtav1"));
        assert_eq!(arg_after(&p.args, "-c:a"), Some("libopus"));
        assert!(!p.args.iter().any(|a| a == "-movflags" || a == "-rc"));
    }

    #[test]
    fn gif_uses_palette_and_no_audio() {
        let mut o = opts(Mode::TargetSize);
        o.format = Format::Gif;
        o.crop = Some([0, 0, 1280, 720]);
        let p = plan(Path::new("in.mp4"), Path::new("out.gif"), &src(), 0.0, 5.0, &o);
        let vf = arg_after(&p.args, "-vf").unwrap();
        assert!(vf.starts_with("crop=1280:720:0:0,fps=15,scale=480:270:flags=lanczos,split"), "{vf}");
        assert!(vf.contains("palettegen") && vf.contains("paletteuse"));
        assert!(p.args.contains(&"-an".to_string()));
        assert!(p.summary.starts_with("480×270"));
    }

    #[test]
    fn gif_preview_writes_gif_and_raw_frames_from_one_graph() {
        let mut o = opts(Mode::TargetSize);
        o.format = Format::Gif;
        let (args, gg) = gif_preview(Path::new("in.mp4"), Path::new("p.gif"), &src(), 1.0, 3.0, &o);
        assert_eq!((gg.width, gg.height), (480, 270));
        let fc = arg_after(&args, "-filter_complex").unwrap();
        assert!(fc.starts_with("[0:v:0]fps=15,scale=480:270") && fc.ends_with("split[g][r]"), "{fc}");
        assert_eq!(arg_after(&args, "-pix_fmt"), Some("pal8"));
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
        assert!(args.contains(&"p.gif".to_string()));
    }

    #[test]
    fn gif_speed_colors_dither_and_loop() {
        let mut o = opts(Mode::TargetSize);
        o.format = Format::Gif;
        o.gif = GifOptions { fps: 20, width: 0, speed: 2.0, colors: 64, dither: Dither::None, loop_forever: false, palette_per_frame: true };
        let p = plan(Path::new("in.mp4"), Path::new("out.gif"), &src(), 0.0, 6.0, &o);
        let vf = arg_after(&p.args, "-vf").unwrap();
        assert!(vf.starts_with("setpts=PTS/2,fps=20,split"), "{vf}");
        assert!(vf.contains("max_colors=64:stats_mode=single") && vf.contains("dither=none") && vf.ends_with(":new=1"), "{vf}");
        assert_eq!(arg_after(&p.args, "-loop"), Some("-1"));
        // The cut is still 6 s of source, taken before the speed change.
        assert_eq!(arg_after(&p.args, "-t"), Some("6.000"));
        assert!(p.summary.contains("3.0 s long"), "{}", p.summary);
    }
}
