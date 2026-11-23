use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub hotkey: String,
    /// Capture frame rate; 0 = the recorded monitor's refresh rate.
    pub fps: u32,
    /// Countdown between selecting the region and the recording starting.
    pub start_delay_secs: u32,
    pub capture_system_audio: bool,
    pub draw_mouse: bool,
    /// NVENC constant-quality value for the raw recording (lower = better). 16–20 is visually lossless.
    pub record_quality: u32,
    /// Manual A/V sync correction; positive delays audio.
    pub audio_offset_ms: i32,
    pub open_editor_after_recording: bool,
    pub output_folder: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        let videos = std::env::var_os("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Videos"))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            hotkey: "Alt+Shift+R".into(),
            fps: 60,
            start_delay_secs: 0,
            capture_system_audio: true,
            draw_mouse: true,
            record_quality: 18,
            audio_offset_ms: 0,
            open_editor_after_recording: true,
            output_folder: videos.join("Clipikon"),
        }
    }
}

/// Editor view preferences. A file of their own: the tray process rewrites settings.json from its copy.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPrefs {
    /// How the preview is scaled up when the view is larger than the video.
    pub upscaler: Upscaler,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upscaler {
    Bilinear,
    /// AMD FSR 1: edge-adaptive upscale + sharpen.
    Fsr,
    /// FSRCNNX: a small neural network that doubles the luma; FSR 1 covers what's left beyond 2×.
    #[default]
    Fsrcnnx,
}

impl Upscaler {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bilinear => "Bilinear",
            Self::Fsr => "FSR 1",
            Self::Fsrcnnx => "FSRCNNX",
        }
    }
}

impl EditorPrefs {
    fn path() -> PathBuf {
        Settings::path().with_file_name("editor.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), json);
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        appdata.join("Clipikon").join("settings.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
