//! Locating and driving ffmpeg: linked into this exe (run as a child process of itself) or an ffmpeg.exe.
//! All heavy lifting (capture, encode, decode) happens in ffmpeg.

use regex::Regex;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
}

/// Set in the environment of a child clipikon.exe to make it run as ffmpeg (see `run_linked`). An environment
/// variable rather than an argument, because ffmpeg re-reads the raw command line on Windows.
#[cfg(linked_ffmpeg)]
pub const AS_FFMPEG_ENV: &str = "CLIPIKON_AS_FFMPEG";

/// The ffmpeg to run: an `ffmpeg.exe`, or this executable with FFmpeg linked in.
#[derive(Clone, Debug)]
pub struct Exe {
    pub path: PathBuf,
    pub linked: bool,
}

/// Lookup order: an ffmpeg.exe placed next to clipikon.exe (lets users swap in their own FFmpeg), then the
/// linked-in copy, then PATH and package-manager shims.
pub fn find() -> Option<Exe> {
    static FOUND: OnceLock<Option<Exe>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let external = |path: PathBuf| Exe { path, linked: false };
            let mut candidates = Vec::new();
            if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
                candidates.push(dir.join("ffmpeg.exe"));
                candidates.push(dir.join("ffmpeg").join("ffmpeg.exe"));
                candidates.push(dir.join("ffmpeg").join("bin").join("ffmpeg.exe"));
            }
            if let Some(p) = candidates.iter().find(|p| p.is_file()) {
                return Some(external(p.clone()));
            }
            #[cfg(linked_ffmpeg)]
            if let Ok(path) = std::env::current_exe() {
                return Some(Exe { path, linked: true });
            }
            if let Some(path) = std::env::var_os("PATH") {
                candidates.extend(std::env::split_paths(&path).map(|p| p.join("ffmpeg.exe")));
            }
            // Package-manager shims; the running process may predate their PATH update.
            if let Some(home) = std::env::var_os("USERPROFILE") {
                candidates.push(PathBuf::from(&home).join("scoop").join("shims").join("ffmpeg.exe"));
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                candidates.push(PathBuf::from(local).join("Microsoft").join("WinGet").join("Links").join("ffmpeg.exe"));
            }
            candidates.into_iter().find(|p| p.is_file()).map(external)
        })
        .clone()
}

#[cfg(linked_ffmpeg)]
unsafe extern "C" {
    /// fftools' `main`, renamed by ffmpeg-build/build.sh.
    fn ffmpeg_main(argc: std::ffi::c_int, argv: *mut *mut std::ffi::c_char) -> std::ffi::c_int;
    /// The C runtime's exit: unlike `std::process::exit` (ExitProcess), it flushes ffmpeg's buffered stdout.
    fn exit(code: std::ffi::c_int) -> !;
}

/// Runs the linked-in ffmpeg on this process's command line and exits with its status.
#[cfg(linked_ffmpeg)]
pub fn run_linked() -> ! {
    use std::ffi::{CString, c_char};
    // ffmpeg rebuilds argv from GetCommandLineW itself; this copy is only for the C signature.
    let args: Vec<CString> =
        std::env::args_os().map(|a| CString::new(a.to_string_lossy().into_owned()).unwrap_or_default()).collect();
    let mut argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
    argv.push(std::ptr::null_mut());
    unsafe { exit(ffmpeg_main(args.len() as _, argv.as_mut_ptr())) }
}

#[cfg(linked_ffmpeg)]
pub const LICENSE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ffmpeg-build/out/FFmpeg-LICENSE.txt"));

/// Text for the About box.
pub fn about() -> String {
    #[cfg(linked_ffmpeg)]
    let bundled = format!(
        "Includes FFmpeg {v} (https://ffmpeg.org), licensed under the GNU LGPL v2.1 or later and statically \
         linked. Source: https://ffmpeg.org/releases/ffmpeg-{v}.tar.xz, built with ffmpeg-build/build.cmd from \
         Clipikon's source, which you can use to relink Clipikon with a modified FFmpeg.\n\nFFmpeg is built with \
         SVT-AV1 (BSD-3-Clause-Clear, AOMedia patent license) and Opus (BSD-3-Clause).",
        v = env!("CLIPIKON_FFMPEG_VERSION")
    );
    #[cfg(not(linked_ffmpeg))]
    let bundled = "Uses FFmpeg (https://ffmpeg.org) as a separate program.".to_string();
    let using = match find() {
        Some(e) if e.linked => "Using the built-in FFmpeg. To use a different one, put ffmpeg.exe next to clipikon.exe.".into(),
        Some(e) => format!("Using {}", e.path.display()),
        None => "FFmpeg was not found.".into(),
    };
    let fsrcnnx = "The editor's FSRCNNX upscaler (igv, https://github.com/igv/FSRCNN-TensorFlow) is licensed under \
                   the GNU LGPL v3 or later; its shader ships unmodified in Clipikon's source (src/shaders).";
    format!("Clipikon {}\n\n{bundled}\n\n{fsrcnnx}\n\n{using}", env!("CARGO_PKG_VERSION"))
}

pub fn require() -> Result<Exe, String> {
    find().ok_or_else(|| {
        "ffmpeg.exe not found. Put it next to clipikon.exe (or in an 'ffmpeg' folder beside it), or add it to PATH."
            .to_string()
    })
}

/// A hidden ffmpeg process with all pipes captured.
pub fn spawn<S: AsRef<std::ffi::OsStr>>(args: &[S], stdin: bool) -> Result<Child, String> {
    let exe = require()?;
    let mut cmd = Command::new(&exe.path);
    #[cfg(linked_ffmpeg)]
    if exe.linked {
        cmd.env(AS_FFMPEG_ENV, "1");
    }
    // SVT-AV1 prints a configuration banner to stderr unless limited to warnings and errors.
    cmd.env("SVT_LOG", "2");
    cmd.args(args)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Failed to start ffmpeg: {e}"))
}

/// Collects stderr on a background thread; returns the shared buffer.
pub fn collect_stderr(child: &mut Child) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let b = buf.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut s = b.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
        });
    }
    buf
}

pub fn tail(s: &str, lines: usize) -> String {
    let all: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `out_time_us=…` lines of `-progress` output, as seconds.
pub fn parse_out_time(line: &str) -> Option<f64> {
    let us: i64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    (us > 0).then(|| us as f64 / 1e6)
}

/// Runs ffmpeg to completion, reporting encoded seconds. `-progress` is added here.
/// Setting `cancel` kills the process and returns `Err("cancelled")`.
pub fn run(args: &[String], mut progress: impl FnMut(f64), cancel: &Arc<AtomicBool>) -> Result<(), String> {
    let mut full: Vec<String> =
        ["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    full.extend_from_slice(args);
    let mut child = spawn(&full, false)?;
    let stderr = collect_stderr(&mut child);
    let stdout = child.stdout.take().unwrap();

    // Watchdog so cancellation works even while ffmpeg is silent.
    let done = Arc::new(AtomicBool::new(false));
    let pid = child.id();
    let (done2, cancel2) = (done.clone(), cancel.clone());
    let watchdog = std::thread::spawn(move || {
        while !done2.load(Ordering::Relaxed) {
            if cancel2.load(Ordering::Relaxed) {
                kill_pid(pid);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(t) = parse_out_time(&line) {
            progress(t);
        }
    }
    let status = child.wait();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            // Give the stderr thread a moment to drain.
            std::thread::sleep(Duration::from_millis(50));
            Err(format!("ffmpeg failed ({s}):\n{}", tail(&stderr.lock().unwrap(), 12)))
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Duration, size, fps and audio presence from `ffmpeg -i` (no ffprobe needed).
pub fn probe(file: &Path) -> Result<MediaInfo, String> {
    let mut child = spawn(&[std::ffi::OsStr::new("-hide_banner"), "-nostdin".as_ref(), "-i".as_ref(), file.as_os_str()], false)?;
    let mut text = String::new();
    child.stderr.take().unwrap().read_to_string(&mut text).map_err(|e| e.to_string())?;
    let _ = child.wait();

    static RX: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    let (dur_rx, vid_rx, fps_rx) = RX.get_or_init(|| {
        (
            Regex::new(r"Duration: (\d+):(\d\d):(\d\d\.\d+)").unwrap(),
            Regex::new(r"Stream #\d+:\d+.*?Video:.*?, (\d{2,5})x(\d{2,5})[^\n]*").unwrap(),
            Regex::new(r"([\d.]+) fps").unwrap(),
        )
    });
    let bad = || format!("Can't read video info:\n{}", tail(&text, 12));
    let d = dur_rx.captures(&text).ok_or_else(bad)?;
    let v = vid_rx.captures(&text).ok_or_else(bad)?;
    let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    Ok(MediaInfo {
        duration: num(&d[1]) * 3600.0 + num(&d[2]) * 60.0 + num(&d[3]),
        width: v[1].parse().unwrap_or(0),
        height: v[2].parse().unwrap_or(0),
        fps: fps_rx.captures(&v[0]).map(|c| num(&c[1])).filter(|f| *f > 0.0).unwrap_or(60.0),
        has_audio: text.contains("Audio:"),
    })
}
