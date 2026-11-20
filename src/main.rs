#![windows_subsystem = "windows"]
//! Clipikon: press a hotkey, drag a region, record it at 60+ fps with NVENC, then trim and compress.
//!
//! Modes:
//!   clipikon                        tray daemon (hotkey, overlay, recorder)
//!   clipikon edit <video>           trim/export window (also: drop a video onto the exe)
//!   clipikon quick-menu             hold-hotkey settings flyout, driven by the daemon (see quickmenu.rs)
//!   clipikon --record-test <secs> [x y w h]   records display 0 headlessly, for diagnostics
//!   clipikon --ui-test              region picker + countdown/recording pills, without recording
//!
//! With FFmpeg linked in (see build.rs), the app also starts itself as its ffmpeg worker process: see
//! `ffmpeg::AS_FFMPEG_ENV`.

mod audio;
mod autostart;
mod clipboard;
mod daemon;
mod displays;
mod editor;
mod export;
mod fluent;
mod ffmpeg;
mod frame;
mod icon_art;
mod layered;
mod overlay;
mod preview;
mod quickmenu;
mod recorder;
mod settings;
mod video_view;
mod win;

use std::path::PathBuf;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

fn main() {
    #[cfg(linked_ffmpeg)]
    if std::env::var_os(ffmpeg::AS_FFMPEG_ENV).is_some() {
        ffmpeg::run_linked(); // never returns
    }
    // Physical-pixel coordinates everywhere: the selected region must map 1:1 to captured pixels.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, file] if cmd == "edit" => open_editor(PathBuf::from(file)),
        [cmd] if cmd == "quick-menu" => {
            let _ = quickmenu::run();
        }
        [cmd, rest @ ..] if cmd == "--record-test" => record_test(rest),
        [cmd] if cmd == "--ui-test" => daemon::ui_test(),
        [file] if PathBuf::from(file).is_file() => open_editor(PathBuf::from(file)),
        _ => daemon::run(),
    }
}

fn open_editor(file: PathBuf) {
    if let Err(e) = editor::run(file) {
        win::message_box(&e, true);
    }
}

/// Local time as `YYYY-MM-DD_HH-MM-SS` for file names.
pub fn timestamp_name() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn record_test(args: &[String]) {
    let log = std::env::temp_dir().join("Clipikon").join("record-test.txt");
    let _ = std::fs::create_dir_all(log.parent().unwrap());
    let result = (|| -> Result<String, String> {
        let secs: f64 = args.first().and_then(|s| s.parse().ok()).ok_or("usage: --record-test <secs> [x y w h]")?;
        let d = displays::enumerate().into_iter().next().ok_or("no displays")?;
        let n: Vec<i32> = args[1..].iter().filter_map(|s| s.parse().ok()).collect();
        let region = if n.len() == 4 {
            displays::CaptureRegion::from_drag(&d, n[0], n[1], n[0] + n[2], n[1] + n[3])
        } else {
            displays::CaptureRegion::full(&d)
        };
        let rec = recorder::Recorder::start(&region, &settings::Settings::load())?;
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        let file = rec.stop()?;
        Ok(format!("OK {}\nregion {region:?}", file.display()))
    })();
    let _ = std::fs::write(&log, result.unwrap_or_else(|e| format!("FAIL {e}")));
}
