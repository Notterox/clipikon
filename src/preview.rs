//! Video preview for the editor: ffmpeg decodes RGBA frames, scaled to the view (never above the source),
//! into a pipe.
//! Stills (scrubbing) use one short-lived process per request, latest request wins; playback streams
//! from a seek point and is paced here against the wall clock.

use eframe::egui;
use std::io::Read;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub time: f64,
}

struct Playback {
    child: Arc<Mutex<Child>>,
    stop: Arc<AtomicBool>,
}

pub struct Preview {
    pub width: u32,
    pub height: u32,
    file: PathBuf,
    fps: f64,
    duration: f64,
    src_w: u32,
    src_h: u32,
    latest: Arc<Mutex<Option<Frame>>>,
    finished: Arc<AtomicBool>,
    still_tx: mpsc::Sender<(f64, u32, u32)>,
    playback: Option<Playback>,
    ctx: egui::Context,
}

impl Preview {
    /// Decodes at most `max_width` wide to start with; [`Preview::resize`] follows the view.
    pub fn new(ctx: egui::Context, file: PathBuf, src_w: u32, src_h: u32, fps: f64, duration: f64, max_width: u32) -> Self {
        let latest: Arc<Mutex<Option<Frame>>> = Arc::default();
        let (still_tx, still_rx) = mpsc::channel::<(f64, u32, u32)>();
        {
            let (file, latest, ctx) = (file.clone(), latest.clone(), ctx.clone());
            std::thread::spawn(move || {
                while let Ok(mut req) = still_rx.recv() {
                    while let Ok(newer) = still_rx.try_recv() {
                        req = newer;
                    }
                    let (t, width, height) = req;
                    if let Some(rgba) = decode_still(&file, t, width, height) {
                        *latest.lock().unwrap() = Some(Frame { rgba, width, height, time: t });
                        ctx.request_repaint();
                    }
                }
            });
        }
        let (width, height) = fit(src_w, src_h, max_width);
        Self { width, height, src_w, src_h, file, fps, duration, latest, finished: Arc::default(), still_tx, playback: None, ctx }
    }

    /// The decode size for a view that needs `max_width` pixels across the whole frame: that width rounded
    /// up to a step (so small window resizes don't restart decoding), never above the source.
    pub fn size_for(&self, max_width: u32) -> (u32, u32) {
        fit(self.src_w, self.src_h, max_width.div_ceil(64) * 64)
    }

    /// Changes the decode size; takes effect with the next `seek` or `play`.
    pub fn resize(&mut self, (width, height): (u32, u32)) {
        self.width = width;
        self.height = height;
    }

    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// Shows the frame at `t` (stops playback).
    pub fn seek(&mut self, t: f64) {
        self.stop();
        let t = t.clamp(0.0, (self.duration - 1.0 / self.fps).max(0.0));
        let _ = self.still_tx.send((t, self.width, self.height));
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn play(&mut self, from: f64, to: f64) {
        self.stop();
        self.finished.store(false, Ordering::Relaxed);
        let (w, h, fps) = (self.width, self.height, self.fps);
        let args: Vec<String> = [
            "-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &format!("{from:.3}"), "-i", &self.file.to_string_lossy(),
            "-t", &format!("{:.3}", (to - from).max(0.05)), "-an", "-vf", &format!("scale={w}:{h}"),
            "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let Ok(mut child) = crate::ffmpeg::spawn(&args, false) else { return };
        let mut out = child.stdout.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (latest, finished, ctx, stop2) = (self.latest.clone(), self.finished.clone(), self.ctx.clone(), stop.clone());
        std::thread::spawn(move || {
            let size = (w * h * 4) as usize;
            let started = Instant::now();
            for k in 0u64.. {
                let mut buf = vec![0u8; size];
                if out.read_exact(&mut buf).is_err() || stop2.load(Ordering::Relaxed) {
                    break;
                }
                let due = Duration::from_secs_f64(k as f64 / fps);
                if let Some(wait) = due.checked_sub(started.elapsed()) {
                    std::thread::sleep(wait);
                }
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                *latest.lock().unwrap() = Some(Frame { rgba: buf, width: w, height: h, time: from + k as f64 / fps });
                ctx.request_repaint();
            }
            if !stop2.load(Ordering::Relaxed) {
                finished.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            }
        });
        self.playback = Some(Playback { child: Arc::new(Mutex::new(child)), stop });
    }

    pub fn stop(&mut self) {
        if let Some(p) = self.playback.take() {
            p.stop.store(true, Ordering::Relaxed);
            let mut c = p.child.lock().unwrap();
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `src` scaled down to at most `max_width` wide, keeping its aspect, in even sizes.
fn fit(src_w: u32, src_h: u32, max_width: u32) -> (u32, u32) {
    let width = (src_w.min(max_width.max(2)) & !1).max(2);
    let height = ((src_h as f64 * width as f64 / src_w as f64 / 2.0).round() as u32 * 2).max(2);
    (width, height)
}

fn decode_still(file: &std::path::Path, t: f64, w: u32, h: u32) -> Option<Vec<u8>> {
    let args: Vec<String> = [
        "-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &format!("{t:.3}"), "-i", &file.to_string_lossy(),
        "-frames:v", "1", "-an", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut child = crate::ffmpeg::spawn(&args, false).ok()?;
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    child.stdout.take()?.read_to_end(&mut buf).ok()?;
    let _ = child.wait();
    (buf.len() == (w * h * 4) as usize).then_some(buf)
}
