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
    /// Where recordings are saved; unset (null) follows the Videos library, looked up each time.
    pub output_folder: Option<PathBuf>,
    pub screenshot_hotkey: String,
    /// Where screenshots are saved (they are also copied to the clipboard); unset (null) follows Windows'
    /// Screenshots folder, looked up each time.
    pub screenshot_folder: Option<PathBuf>,
    /// Draw the mouse cursor into screenshots.
    pub screenshot_cursor: bool,
    /// Seconds between picking the area and taking the screenshot (then taken live, not from the frozen screen).
    pub screenshot_delay_secs: u32,
    /// Open each screenshot in the editor right away (otherwise its notification does).
    pub screenshot_open_editor: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Shift+R".into(),
            fps: 60,
            start_delay_secs: 0,
            capture_system_audio: true,
            draw_mouse: true,
            record_quality: 18,
            audio_offset_ms: 0,
            open_editor_after_recording: true,
            output_folder: None,
            screenshot_hotkey: "Alt+Shift+S".into(),
            screenshot_folder: None,
            screenshot_cursor: false,
            screenshot_delay_secs: 0,
            screenshot_open_editor: false,
        }
    }
}

/// Editor view preferences. A file of their own: the tray process rewrites settings.json from its copy.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPrefs {
    /// How the preview is scaled up when the view is larger than the video.
    pub upscaler: Upscaler,
    /// How much the screenshot editor smooths pen and highlighter strokes.
    pub smoothing: Smoothing,
    /// The screenshot editor's tools as last left (tool, colors, sizes, fill), in its own format.
    pub image_tools: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Smoothing {
    Off,
    Low,
    #[default]
    Medium,
    High,
}

impl Smoothing {
    pub const ALL: [Smoothing; 4] = [Smoothing::Off, Smoothing::Low, Smoothing::Medium, Smoothing::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
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
        let s: Self = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let before = (s.output_folder.clone(), s.screenshot_folder.clone());
        let s = s.unfixed();
        if before != (s.output_folder.clone(), s.screenshot_folder.clone()) {
            s.save(); // so the file shows the folders as following Windows
        }
        s
    }

    fn unfixed(mut self) -> Self {
        let s = &mut self;
        // Older settings hold the default folders as fixed paths; those follow the libraries again.
        let home = folders::home();
        if s.output_folder.as_ref().is_some_and(|f| f.as_os_str().is_empty() || *f == home.join("Videos").join("Clipikon")) {
            s.output_folder = None;
        }
        let old_shots = home.join("Pictures").join("Screenshots").join("Clipikon");
        if s.screenshot_folder.as_ref().is_some_and(|f| f.as_os_str().is_empty() || *f == old_shots) {
            s.screenshot_folder = None;
        }
        self
    }

    /// The recordings folder, with an unset one resolved now (a library can move).
    pub fn recordings_dir(&self) -> PathBuf {
        self.output_folder.clone().unwrap_or_else(|| folders::videos().join("Clipikon"))
    }

    /// The screenshots folder, with an unset one resolved now.
    pub fn screenshots_dir(&self) -> PathBuf {
        self.screenshot_folder.clone().unwrap_or_else(|| folders::screenshots().join("Clipikon"))
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

/// The user's media folders as Windows has them: a library's save location, else the known folder (which
/// follows a move to another drive or OneDrive), else the plain profile path.
mod folders {
    use std::path::PathBuf;
    use windows::core::{GUID, PWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        IShellItem, IShellLibrary, SHGetKnownFolderPath, ShellLibrary, DSFT_DETECT, FOLDERID_Pictures,
        FOLDERID_PicturesLibrary, FOLDERID_Screenshots, FOLDERID_Videos, FOLDERID_VideosLibrary, KF_FLAG_DEFAULT,
        SIGDN_FILESYSPATH,
    };

    pub fn home() -> PathBuf {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn videos() -> PathBuf {
        library(&FOLDERID_VideosLibrary).or_else(|| known(&FOLDERID_Videos)).unwrap_or_else(|| home().join("Videos"))
    }

    pub fn pictures() -> PathBuf {
        library(&FOLDERID_PicturesLibrary).or_else(|| known(&FOLDERID_Pictures)).unwrap_or_else(|| home().join("Pictures"))
    }

    /// Where Windows keeps its own screenshots (Win+PrtScn), else Screenshots in the pictures folder.
    pub fn screenshots() -> PathBuf {
        known(&FOLDERID_Screenshots).unwrap_or_else(|| pictures().join("Screenshots"))
    }

    fn known(id: &GUID) -> Option<PathBuf> {
        unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok().and_then(|p| take(p)) }
    }

    fn library(id: &GUID) -> Option<PathBuf> {
        unsafe {
            // Fine on a thread that already has COM in another mode; only undo what this call did.
            let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let path = (|| {
                let lib: IShellLibrary = CoCreateInstance(&ShellLibrary, None, CLSCTX_INPROC_SERVER).ok()?;
                lib.LoadLibraryFromKnownFolder(id, 0).ok()?; // STGM_READ
                let item: IShellItem = lib.GetDefaultSaveFolder(DSFT_DETECT).ok()?;
                take(item.GetDisplayName(SIGDN_FILESYSPATH).ok()?)
            })();
            if inited {
                CoUninitialize();
            }
            path
        }
    }

    /// Copies a shell-allocated string and frees it.
    unsafe fn take(p: PWSTR) -> Option<PathBuf> {
        if p.is_null() {
            return None;
        }
        let s = unsafe { p.to_string() }.ok();
        unsafe { CoTaskMemFree(Some(p.0 as _)) };
        s.filter(|s| !s.is_empty()).map(PathBuf::from)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn default_folders_follow_windows() {
            use super::super::Settings;
            let home = super::home();
            let old = serde_json::json!({
                "output_folder": home.join("Videos").join("Clipikon"),
                "screenshot_folder": home.join("Pictures").join("Screenshots").join("Clipikon"),
            });
            let s = serde_json::from_value::<Settings>(old).unwrap().unfixed();
            assert_eq!((s.output_folder, s.screenshot_folder), (None, None));
            let custom = serde_json::json!({ "output_folder": r"D:\Rec", "screenshot_folder": null });
            let s = serde_json::from_value::<Settings>(custom).unwrap().unfixed();
            assert_eq!(s.output_folder, Some(r"D:\Rec".into()));
            assert_eq!(s.screenshot_folder, None);
            assert!(s.screenshots_dir().ends_with("Clipikon"));
            assert!(serde_json::to_string(&Settings::default()).unwrap().contains(r#""output_folder":null"#));
        }

        #[test]
        #[ignore = "prints this machine's folders"]
        fn shows_folders() {
            println!("videos {:?}\npictures {:?}\nscreenshots {:?}", super::videos(), super::pictures(), super::screenshots());
            use windows::Win32::UI::Shell::*;
            println!("libraries {:?} {:?}", super::library(&FOLDERID_VideosLibrary), super::library(&FOLDERID_PicturesLibrary));
            println!("known {:?} {:?} {:?}", super::known(&FOLDERID_Videos), super::known(&FOLDERID_Pictures), super::known(&FOLDERID_Screenshots));
        }
    }
}
