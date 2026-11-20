//! One recording session.
//!
//! Video: ffmpeg `ddagrab` (Desktop Duplication, cropped on the GPU) → `h264_nvenc`, so frames never leave
//! video memory. Audio: WASAPI loopback to WAV in-process. Both are stamped against `Instant` and muxed into
//! an MP4 with the measured offset when stopped.

use crate::audio::AudioCapture;
use crate::displays::CaptureRegion;
use crate::ffmpeg;
use crate::settings::Settings;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub struct Recorder {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    progress_thread: Option<std::thread::JoinHandle<()>>,
    /// Earliest estimate of when the first video frame was captured.
    video_start: Arc<Mutex<Option<Instant>>>,
    audio: Option<AudioCapture>,
    temp_dir: PathBuf,
    video_path: PathBuf,
    audio_path: PathBuf,
    settings: Settings,
    exited: Arc<AtomicBool>,
}

impl Recorder {
    /// Starts capture and blocks until the first frame is encoded (or ffmpeg fails).
    pub fn start(region: &CaptureRegion, settings: &Settings) -> Result<Self, String> {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let temp_dir = std::env::temp_dir().join("Clipikon").join(stamp.to_string());
        std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
        let video_path = temp_dir.join("video.mkv");
        let audio_path = temp_dir.join("audio.wav");

        let audio = if settings.capture_system_audio {
            AudioCapture::start(audio_path.clone()).map_err(|e| eprintln!("audio unavailable: {e}")).ok()
        } else {
            None
        };

        let fps = match (settings.fps, region.display.refresh_hz) {
            (0, 0) => 60, // "refresh rate", but the monitor didn't report one
            (0, hz) => hz,
            (f, _) => f,
        };
        // NVENC converts the RGB desktop frames with BT.601 limited range (and writes that into the H.264 VUI);
        // setparams makes the container tags say the same instead of ddagrab's RGB/full range.
        let grab = format!(
            "ddagrab=output_idx={}:framerate={fps}:draw_mouse={}:offset_x={}:offset_y={}:video_size={}x{},\
             setparams=colorspace=bt470bg:range=tv:color_primaries=bt709:color_trc=bt709[v]",
            region.display.output_index,
            settings.draw_mouse as u8,
            region.x,
            region.y,
            region.width,
            region.height
        );
        let args: Vec<String> = [
            "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-stats_period", "0.1",
            "-filter_complex", &grab, "-map", "[v]",
            // Near-lossless intermediate; compression happens at export. No B-frames / zero latency keeps the
            // capture→mux delay tiny, which the A/V offset measurement relies on.
            "-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr",
            "-cq", &settings.record_quality.to_string(), "-b:v", "0", "-bf", "0", "-zerolatency", "1", "-delay", "0",
            "-g", &fps.to_string(), "-y", &video_path.to_string_lossy(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let mut child = ffmpeg::spawn(&args, true)?;
        set_above_normal_priority(&child);
        let stderr = ffmpeg::collect_stderr(&mut child);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();

        let video_start: Arc<Mutex<Option<Instant>>> = Arc::default();
        let exited = Arc::new(AtomicBool::new(false));
        let (first_tx, first_rx) = mpsc::channel::<()>();
        let (vs, ex) = (video_start.clone(), exited.clone());
        let progress_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(t) = ffmpeg::parse_out_time(&line) {
                    // Wall time minus encoded duration ≥ true capture start; the minimum converges on it.
                    if let Some(est) = Instant::now().checked_sub(Duration::from_secs_f64(t)) {
                        let mut v = vs.lock().unwrap();
                        if v.is_none_or(|cur| est < cur) {
                            *v = Some(est);
                        }
                    }
                    let _ = first_tx.send(());
                }
            }
            ex.store(true, Ordering::SeqCst); // stdout closes when ffmpeg exits
        });

        let mut rec = Self {
            child,
            stdin,
            stderr,
            progress_thread: Some(progress_thread),
            video_start,
            audio,
            temp_dir,
            video_path,
            audio_path,
            settings: settings.clone(),
            exited,
        };
        if first_rx.recv_timeout(Duration::from_secs(10)).is_err() {
            let _ = rec.child.kill();
            let _ = rec.child.wait();
            if let Some(a) = rec.audio.take() {
                let _ = a.stop();
            }
            std::thread::sleep(Duration::from_millis(100));
            let err = ffmpeg::tail(&rec.stderr.lock().unwrap(), 12);
            let _ = std::fs::remove_dir_all(&rec.temp_dir);
            return Err(format!("Recording failed to start.\n{err}"));
        }
        Ok(rec)
    }

    /// True if ffmpeg died on its own (e.g. display mode change); call `stop` to salvage the file.
    pub fn has_failed(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn error_text(&self) -> String {
        ffmpeg::tail(&self.stderr.lock().unwrap(), 12)
    }

    /// Stops capture and writes the final MP4 into the output folder. Returns its path.
    pub fn stop(mut self) -> Result<PathBuf, String> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"q"); // graceful stop: ffmpeg finalizes the file
            let _ = stdin.flush();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.wait();
        if let Some(t) = self.progress_thread.take() {
            let _ = t.join();
        }
        let audio_start = self.audio.take().and_then(|a| a.stop().ok().flatten());
        let video_start = *self.video_start.lock().unwrap();

        if std::fs::metadata(&self.video_path).map(|m| m.len()).unwrap_or(0) == 0 {
            return Err(format!("Nothing was recorded.\n{}", self.error_text()));
        }

        std::fs::create_dir_all(&self.settings.output_folder).map_err(|e| e.to_string())?;
        let output = self.settings.output_folder.join(format!("Clipikon_{}.mp4", crate::timestamp_name()));

        let mut args: Vec<String> = vec!["-i".into(), self.video_path.to_string_lossy().into()];
        if let (Some(a), Some(v)) = (audio_start, video_start) {
            let offset = signed_secs(a, v) + self.settings.audio_offset_ms as f64 / 1000.0;
            let align = if offset >= 0.0 {
                format!("adelay={:.0}:all=1", offset * 1000.0)
            } else {
                format!("atrim=start={:.3},asetpts=PTS-STARTPTS", -offset)
            };
            for s in [
                "-i", &self.audio_path.to_string_lossy(), "-map", "0:v", "-map", "1:a",
                "-af", &format!("{align},apad"), "-c:a", "aac", "-b:a", "192k", "-ac", "2", "-shortest",
            ] {
                args.push(s.to_string());
            }
        }
        for s in ["-c:v", "copy", "-movflags", "+faststart", &output.to_string_lossy()] {
            args.push(s.to_string());
        }

        if let Err(e) = ffmpeg::run(&args, |_| {}, &Arc::new(AtomicBool::new(false))) {
            // Keep the raw capture rather than lose it.
            let rescue = output.with_extension("mkv");
            let _ = std::fs::rename(&self.video_path, &rescue);
            return Err(format!("Couldn't finalize the recording; raw video saved to {}.\n{e}", rescue.display()));
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
        Ok(output)
    }
}

/// `a - b` in seconds, allowing negative results.
fn signed_secs(a: Instant, b: Instant) -> f64 {
    match a.checked_duration_since(b) {
        Some(d) => d.as_secs_f64(),
        None => -b.duration_since(a).as_secs_f64(),
    }
}

fn set_above_normal_priority(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, SetPriorityClass};
    unsafe {
        let _ = SetPriorityClass(HANDLE(child.as_raw_handle()), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}
