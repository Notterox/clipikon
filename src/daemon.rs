//! Tray daemon: global hotkey → region overlay → recording → editor. Pure Win32, one UI thread.
//! Blocking work (starting/stopping ffmpeg) runs on worker threads and reports back via posted messages
//! carrying a boxed result in LPARAM.

use crate::displays::{self, CaptureRegion};
use crate::recorder::Recorder;
use crate::settings::Settings;
use crate::win::*;
use crate::{frame, overlay};
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const CLASS: PCWSTR = w!("ClipikonMain");
const HOTKEY_ID: i32 = 1;
const FAIL_TIMER: usize = 2;
const HOLD_TIMER: usize = 3;
const COUNTDOWN_TIMER: usize = 4;
/// Holding the hotkey at least this long opens the quick settings flyout instead of the region picker.
const HOLD_MS: u128 = 400;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Idle,
    /// Hotkey is down: released before `HOLD_MS` it's a press (record), held past it the flyout opens.
    Held,
    /// The quick settings flyout is open.
    QuickMenu,
    Selecting,
    Countdown,
    Starting,
    Recording,
    Stopping,
}

struct App {
    settings: Settings,
    state: State,
    recorder: Option<Recorder>,
    taskbar_created: u32,
    /// Exit was requested mid-recording: quit once the file is saved.
    quit_after_stop: bool,
    held_since: std::time::Instant,
    /// Region waiting for the start-delay countdown.
    pending: Option<CaptureRegion>,
    /// The quick settings flyout process (started hidden on hotkey down) and its generation, so a late exit
    /// notice from an earlier one is ignored.
    quick: Option<std::process::Child>,
    quick_gen: u32,
    /// Settings for the next recording only (chosen in the flyout); dropped once it starts or is cancelled.
    once: Option<Settings>,
}

impl App {
    /// What the next recording uses: the flyout's one-off choices, else the saved settings.
    fn capture_settings(&self) -> &Settings {
        self.once.as_ref().unwrap_or(&self.settings)
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Borrow the app state briefly. Never call anything that pumps messages (dialogs, menus) inside `f`.
fn app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(a.borrow_mut().as_mut().expect("app not initialised")))
}

mod menu_id {
    pub const RECORD: usize = 1;
    pub const OPEN_VIDEO: usize = 2;
    pub const OPEN_FOLDER: usize = 3;
    pub const AUDIO: usize = 10;
    pub const CURSOR: usize = 11;
    pub const FPS_BASE: usize = 20; // + index into FPS_CHOICES
    pub const EDIT_SETTINGS: usize = 30;
    pub const RELOAD_SETTINGS: usize = 31;
    pub const ABOUT: usize = 40;
    pub const DELAY_BASE: usize = 50; // + index into DELAY_CHOICES
    pub const EXIT: usize = 99;
}
/// 0 = the monitor's refresh rate.
const FPS_CHOICES: [u32; 5] = [0, 30, 60, 120, 144];
const DELAY_CHOICES: [u32; 4] = [0, 3, 5, 10];

pub fn run() {
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Clipikon.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box("Clipikon is already running (see the tray).", false);
            return;
        }

        if crate::ffmpeg::find().is_none() {
            message_box("ffmpeg.exe was not found. Put it next to clipikon.exe or on PATH; recording won't work until then.", true);
        }

        register_class(CLASS, wnd_proc, IDC_ARROW);
        let hwnd = create_window(WINDOW_EX_STYLE(0), CLASS, WS_OVERLAPPED, RECT::default());
        let settings = Settings::load();
        settings.save(); // writes defaults on first run so the file can be edited
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                settings,
                state: State::Idle,
                recorder: None,
                taskbar_created,
                quit_after_stop: false,
                held_since: std::time::Instant::now(),
                pending: None,
                quick: None,
                quick_gen: 0,
                once: None,
            })
        });
        add_tray_icon(hwnd);
        register_hotkey(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_tray_icon(hwnd);
    }
}

// ---------- hotkey ----------

fn register_hotkey(hwnd: HWND) {
    let gesture = app(|a| a.settings.hotkey.clone());
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    }
    match parse_hotkey(&gesture) {
        Ok((mods, vk)) => unsafe {
            if RegisterHotKey(Some(hwnd), HOTKEY_ID, mods | MOD_NOREPEAT, vk).is_err() {
                notify(hwnd, &format!("Couldn't register {gesture}: another app may be using it. Change it in settings."));
            } else {
                notify(hwnd, &format!("Press {gesture} to record a region. Hold it for frame rate, delay and audio options."));
            }
        },
        Err(e) => notify(hwnd, &e),
    }
}

/// WM_HOTKEY. When idle, watches the key to tell a press (record, on release) from a hold (the flyout, as soon
/// as `HOLD_MS` passes). The flyout process starts right away, hidden, so it can appear without delay.
fn on_hotkey_down(hwnd: HWND) {
    if app(|a| a.state) != State::Idle {
        return on_hotkey(hwnd);
    }
    app(|a| {
        a.state = State::Held;
        a.held_since = std::time::Instant::now();
    });
    spawn_quick_menu(hwnd);
    unsafe {
        SetTimer(Some(hwnd), HOLD_TIMER, 30, None);
    }
}

fn check_hold(hwnd: HWND) {
    let (gesture, since) = app(|a| (a.settings.hotkey.clone(), a.held_since));
    let vk = parse_hotkey(&gesture).map(|(_, vk)| vk).unwrap_or(0);
    let down = vk != 0 && unsafe { GetAsyncKeyState(vk as i32) } < 0;
    let held = since.elapsed().as_millis() >= HOLD_MS;
    if down && !held {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), HOLD_TIMER);
    }
    if !held {
        kill_quick_menu();
        app(|a| a.state = State::Idle);
        return on_hotkey(hwnd);
    }
    // Let the flyout take the foreground: this process may, having just received the hotkey.
    if let Some(pid) = app(|a| a.quick.as_ref().map(|c| c.id())) {
        unsafe {
            let _ = AllowSetForegroundWindow(pid);
        }
    }
    if quick_menu_command("show") {
        app(|a| a.state = State::QuickMenu);
    } else {
        kill_quick_menu();
        app(|a| a.state = State::Idle);
        show_menu(hwnd); // the flyout couldn't start: the tray menu has the same options
    }
}

fn spawn_quick_menu(hwnd: HWND) {
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = Command::new(exe)
        .arg("quick-menu")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else { return };
    let generation = app(|a| {
        a.quick_gen += 1;
        a.quick_gen
    });
    let stdout = child.stdout.take();
    let send = SendHwnd(hwnd.0 as isize);
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = std::io::Read::read_to_string(&mut s, &mut out);
        }
        // "record <settings JSON>": the user chose to record, with these one-off choices.
        let chosen: Option<Settings> =
            out.lines().find_map(|l| l.trim().strip_prefix("record")).and_then(|json| serde_json::from_str(json.trim()).ok());
        post_boxed(send.get(), WM_APP_QUICK_DONE, (generation, chosen));
    });
    app(|a| a.quick = Some(child));
}

/// Sends a line to the flyout; false if it isn't running.
fn quick_menu_command(cmd: &str) -> bool {
    use std::io::Write;
    app(|a| {
        let Some(stdin) = a.quick.as_mut().and_then(|c| c.stdin.as_mut()) else { return false };
        writeln!(stdin, "{cmd}").and_then(|_| stdin.flush()).is_ok()
    })
}

fn kill_quick_menu() {
    if let Some(mut c) = app(|a| a.quick.take()) {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// The flyout closed: if the user chose to record, do so with its choices, for that recording only.
fn on_quick_done(hwnd: HWND, (generation, chosen): (u32, Option<Settings>)) {
    if generation != app(|a| a.quick_gen) || app(|a| a.state) != State::QuickMenu {
        return; // a hidden flyout killed after a short press, or an older one
    }
    if let Some(mut c) = app(|a| a.quick.take()) {
        let _ = c.wait();
    }
    app(|a| a.state = State::Idle);
    let Some(chosen) = chosen else { return };
    app(|a| {
        // Only what the flyout offers; everything else stays as saved.
        let mut s = a.settings.clone();
        s.fps = chosen.fps;
        s.start_delay_secs = chosen.start_delay_secs;
        s.capture_system_audio = chosen.capture_system_audio;
        s.draw_mouse = chosen.draw_mouse;
        a.once = Some(s);
    });
    on_hotkey(hwnd);
}

fn parse_hotkey(gesture: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut vk = None;
    for part in gesture.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "alt" => mods |= MOD_ALT,
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            key => {
                vk = Some(match key {
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => k.to_ascii_uppercase().as_bytes()[0] as u32,
                    k if k.starts_with('f') && k[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) => {
                        VK_F1.0 as u32 + k[1..].parse::<u32>().unwrap() - 1
                    }
                    "printscreen" | "prtsc" | "snapshot" => VK_SNAPSHOT.0 as u32,
                    "pause" => VK_PAUSE.0 as u32,
                    "scrolllock" | "scroll" => VK_SCROLL.0 as u32,
                    "insert" => VK_INSERT.0 as u32,
                    "home" => VK_HOME.0 as u32,
                    "end" => VK_END.0 as u32,
                    "pageup" => VK_PRIOR.0 as u32,
                    "pagedown" => VK_NEXT.0 as u32,
                    "space" => VK_SPACE.0 as u32,
                    _ => return Err(format!("Unknown key '{part}' in hotkey '{gesture}'.")),
                })
            }
        }
    }
    vk.map(|vk| (mods, vk)).ok_or_else(|| format!("Hotkey '{gesture}' has no key."))
}

// ---------- tray ----------

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

thread_local! {
    /// The icon currently in the tray, destroyed when replaced.
    static TRAY_ICON: std::cell::Cell<HICON> = std::cell::Cell::new(HICON::default());
}

fn add_tray_icon(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    nid.hIcon = make_icon(app(|a| a.state == State::Recording));
    copy_wide(&mut nid.szTip, "Clipikon");
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
    replace_tray_icon_handle(nid.hIcon);
}

/// Redraws the tray icon: on recording start/stop, and when the taskbar's theme changes.
fn update_tray_icon(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON;
    nid.hIcon = make_icon(app(|a| a.state == State::Recording));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
    replace_tray_icon_handle(nid.hIcon);
}

fn replace_tray_icon_handle(icon: HICON) {
    let old = TRAY_ICON.with(|c| c.replace(icon));
    if !old.is_invalid() {
        unsafe {
            let _ = DestroyIcon(old);
        }
    }
}

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
}

fn set_tip(hwnd: HWND, tip: &str) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_TIP;
    copy_wide(&mut nid.szTip, tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn notify(hwnd: HWND, text: &str) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_INFO;
    copy_wide(&mut nid.szInfo, text);
    copy_wide(&mut nid.szInfoTitle, "Clipikon");
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Whether the taskbar uses the light theme (it follows the Windows mode, not the app mode).
fn taskbar_is_light() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let mut value: u32 = 0;
    let mut size = 4u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && value != 0
}

/// The tray icon: the single-color brackets in the taskbar's text color, drawn at the tray's exact size; the
/// dot turns red while recording.
fn make_icon(recording: bool) -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let fg = if taskbar_is_light() { [0x1A, 0x1A, 0x1A] } else { [0xFF, 0xFF, 0xFF] };
    let dot = if recording { [0xE5, 0x48, 0x4D] } else { fg };
    let rgba = crate::icon_art::render(size as u32, crate::icon_art::Style::Mono { fg, dot });
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        let px = std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
        for (d, s) in px.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]); // BGRA, straight alpha as icons expect
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        icon
    }
}

fn append_item(m: HMENU, id: usize, text: &str, checked: bool) {
    let t = wide(text);
    let flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
    unsafe {
        let _ = AppendMenuW(m, flags, id, PCWSTR(t.as_ptr()));
    }
}

fn append_submenu(m: HMENU, sub: HMENU, text: &str) {
    let t = wide(text);
    unsafe {
        let _ = AppendMenuW(m, MF_POPUP, sub.0 as usize, PCWSTR(t.as_ptr()));
    }
}

fn append_separator(m: HMENU) {
    unsafe {
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
    }
}

fn fps_label(fps: u32, hz: u32) -> String {
    match (fps, hz) {
        (0, 0) => "Monitor refresh rate".into(),
        (0, hz) => format!("Monitor refresh rate ({hz} fps)"),
        (f, _) => format!("{f} fps"),
    }
}

fn delay_label(secs: u32) -> String {
    if secs == 0 { "Off".into() } else { format!("{secs} seconds") }
}

/// Frame rate, start delay, audio and cursor: shared by the tray menu and the hold-hotkey menu.
fn append_capture_settings(menu: HMENU, s: &Settings) {
    let hz = displays::under_cursor().map(|d| d.refresh_hz).unwrap_or(0);
    unsafe {
        if let Ok(fps) = CreatePopupMenu() {
            for (i, f) in FPS_CHOICES.iter().enumerate() {
                append_item(fps, menu_id::FPS_BASE + i, &fps_label(*f, hz), s.fps == *f);
                if *f == 0 {
                    append_separator(fps);
                }
            }
            let current = if s.fps == 0 { "refresh rate".to_string() } else { format!("{} fps", s.fps) };
            append_submenu(menu, fps, &format!("Frame rate: {current}"));
        }
        if let Ok(delay) = CreatePopupMenu() {
            for (i, d) in DELAY_CHOICES.iter().enumerate() {
                append_item(delay, menu_id::DELAY_BASE + i, &delay_label(*d), s.start_delay_secs == *d);
            }
            append_submenu(menu, delay, &format!("Start delay: {}", delay_label(s.start_delay_secs).to_lowercase()));
        }
    }
    append_item(menu, menu_id::AUDIO, "Record system audio", s.capture_system_audio);
    append_item(menu, menu_id::CURSOR, "Show mouse cursor", s.draw_mouse);
}

/// Applies a capture-settings menu choice; false if `id` isn't one.
fn apply_capture_setting(id: usize) -> bool {
    let fps = menu_id::FPS_BASE..menu_id::FPS_BASE + FPS_CHOICES.len();
    let delay = menu_id::DELAY_BASE..menu_id::DELAY_BASE + DELAY_CHOICES.len();
    app(|a| {
        let s = &mut a.settings;
        match id {
            menu_id::AUDIO => s.capture_system_audio = !s.capture_system_audio,
            menu_id::CURSOR => s.draw_mouse = !s.draw_mouse,
            id if fps.contains(&id) => s.fps = FPS_CHOICES[id - fps.start],
            id if delay.contains(&id) => s.start_delay_secs = DELAY_CHOICES[id - delay.start],
            _ => return false,
        }
        s.save();
        true
    })
}

/// Shows `menu` at the cursor, destroys it, and returns the chosen id (0 = dismissed).
fn track_menu(hwnd: HWND, menu: HMENU) -> usize {
    unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        // Required for the menu to close when clicking elsewhere (documented TrackPopupMenu quirk).
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}

fn show_menu(hwnd: HWND) {
    let s = app(|a| a.settings.clone());
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else { return };
    append_item(menu, menu_id::RECORD, &format!("Record region\t{}", s.hotkey), false);
    append_item(menu, menu_id::OPEN_VIDEO, "Open video to trim…", false);
    append_item(menu, menu_id::OPEN_FOLDER, "Open recordings folder", false);
    append_separator(menu);
    append_capture_settings(menu, &s);
    append_item(menu, menu_id::EDIT_SETTINGS, "Edit settings…", false);
    append_item(menu, menu_id::RELOAD_SETTINGS, "Reload settings", false);
    append_separator(menu);
    append_item(menu, menu_id::ABOUT, "About Clipikon", false);
    append_item(menu, menu_id::EXIT, "Exit", false);
    let cmd = track_menu(hwnd, menu);
    on_menu(hwnd, cmd);
}

fn on_menu(hwnd: HWND, id: usize) {
    if apply_capture_setting(id) {
        return;
    }
    match id {
        menu_id::RECORD => on_hotkey(hwnd),
        menu_id::OPEN_VIDEO => {
            let dir = app(|a| a.settings.output_folder.clone());
            let mut dlg = rfd::FileDialog::new().add_filter("Videos", &["mp4", "mkv", "mov"]);
            if dir.is_dir() {
                dlg = dlg.set_directory(dir);
            }
            if let Some(file) = dlg.pick_file() {
                open_editor(&file);
            }
        }
        menu_id::OPEN_FOLDER => {
            let dir = app(|a| a.settings.output_folder.clone());
            let _ = std::fs::create_dir_all(&dir);
            let _ = Command::new("explorer.exe").arg(dir).spawn();
        }
        menu_id::EDIT_SETTINGS => {
            let _ = Command::new("notepad.exe").arg(Settings::path()).spawn();
        }
        menu_id::RELOAD_SETTINGS => {
            app(|a| a.settings = Settings::load());
            register_hotkey(hwnd);
        }
        menu_id::ABOUT => show_about(),
        menu_id::EXIT => match app(|a| a.state) {
            State::Recording | State::Stopping => {
                app(|a| a.quit_after_stop = true);
                stop_recording(hwnd); // finalize first; on_stopped quits
            }
            _ => unsafe { PostQuitMessage(0) },
        },
        _ => {}
    }
}

fn show_about() {
    let text = crate::ffmpeg::about();
    #[cfg(linked_ffmpeg)]
    unsafe {
        let t = wide(&format!("{text}\n\nView the licenses?"));
        if MessageBoxW(None, PCWSTR(t.as_ptr()), w!("About Clipikon"), MB_YESNO | MB_ICONINFORMATION) == IDYES {
            let file = std::env::temp_dir().join("Clipikon").join("FFmpeg-LICENSE.txt");
            let _ = std::fs::create_dir_all(file.parent().unwrap());
            if std::fs::write(&file, crate::ffmpeg::LICENSE).is_ok() {
                let _ = Command::new("notepad.exe").arg(file).spawn();
            }
        }
    }
    #[cfg(not(linked_ffmpeg))]
    message_box(&text, false);
}

pub fn open_editor(file: &std::path::Path) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).arg("edit").arg(file).spawn();
    }
}

// ---------- recording flow ----------

fn on_hotkey(hwnd: HWND) {
    match app(|a| a.state) {
        State::Idle => {
            let list = displays::enumerate();
            if list.is_empty() {
                message_box("No capturable displays found.", true);
                return;
            }
            let settings = app(|a| {
                a.state = State::Selecting;
                a.capture_settings().clone()
            });
            overlay::select(hwnd, &list, &settings);
        }
        State::Recording => stop_recording(hwnd),
        State::Countdown => cancel_countdown(hwnd),
        // Pressing the hotkey again while the flyout is open records.
        State::QuickMenu => {
            if !quick_menu_command("record") {
                app(|a| a.state = State::Idle);
            }
        }
        _ => {}
    }
}

/// `clipikon --ui-test`: the region picker, then the countdown pill and the recording pill for 3 s each on the
/// selection, without recording anything. For checking the capture UI.
pub fn ui_test() {
    thread_local! {
        static REGION: RefCell<Option<CaptureRegion>> = const { RefCell::new(None) };
    }
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let three_s = std::time::Duration::from_secs(3);
        match msg {
            WM_APP_SELECTED => match unsafe { take_boxed::<Option<overlay::Selection>>(lp) } {
                Some(overlay::Selection { region: r, .. }) => {
                    frame::show(hwnd, &r, Some(std::time::Instant::now() + three_s));
                    REGION.with(|x| *x.borrow_mut() = Some(r));
                    unsafe { SetTimer(Some(hwnd), 1, 3000, None) };
                }
                None => unsafe { PostQuitMessage(0) },
            },
            WM_TIMER if wp.0 == 1 => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), 1);
                    SetTimer(Some(hwnd), 2, 3000, None);
                }
                frame::close();
                if let Some(r) = REGION.with(|x| x.borrow().clone()) {
                    frame::show(hwnd, &r, None);
                }
            }
            WM_TIMER | WM_APP_STOP => {
                frame::close();
                unsafe { PostQuitMessage(0) };
            }
            _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
        LRESULT(0)
    }
    frame::set_capturable(true); // so screenshots of the test show the pill
    register_class(w!("ClipikonUiTest"), proc, IDC_ARROW);
    let hwnd = create_window(WINDOW_EX_STYLE(0), w!("ClipikonUiTest"), WS_OVERLAPPED, RECT::default());
    overlay::select(hwnd, &displays::enumerate(), &Settings::load());
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn post_boxed<T>(hwnd: HWND, msg: u32, value: T) {
    let ptr = Box::into_raw(Box::new(value));
    unsafe {
        if PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(ptr as isize)).is_err() {
            drop(Box::from_raw(ptr));
        }
    }
}

/// Takes back a value sent with `post_boxed`.
unsafe fn take_boxed<T>(lp: LPARAM) -> T {
    unsafe { *Box::from_raw(lp.0 as *mut T) }
}

fn on_selected(hwnd: HWND, picked: Option<overlay::Selection>) {
    let Some(overlay::Selection { region, settings }) = picked else {
        app(|a| {
            a.state = State::Idle;
            a.once = None;
        });
        return;
    };
    // The picker's toolbar choices (starting from any hold-flyout ones) are for this recording only.
    app(|a| a.once = Some(settings));
    let delay = app(|a| a.capture_settings().start_delay_secs);
    if delay > 0 {
        app(|a| {
            a.state = State::Countdown;
            a.pending = Some(region.clone());
        });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(delay as u64);
        frame::show(hwnd, &region, Some(end));
        unsafe {
            SetTimer(Some(hwnd), COUNTDOWN_TIMER, delay * 1000, None);
        }
        return;
    }
    start_recorder(hwnd, region);
}

fn countdown_done(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), COUNTDOWN_TIMER);
    }
    frame::close();
    if let Some(region) = app(|a| a.pending.take()) {
        start_recorder(hwnd, region);
    }
}

fn cancel_countdown(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), COUNTDOWN_TIMER);
    }
    frame::close();
    app(|a| {
        a.pending = None;
        a.once = None;
        a.state = State::Idle;
    });
}

fn start_recorder(hwnd: HWND, region: CaptureRegion) {
    let settings = app(|a| {
        a.state = State::Starting;
        a.once.take().unwrap_or_else(|| a.settings.clone())
    });
    let send = SendHwnd(hwnd.0 as isize);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150)); // let DWM drop the overlay first
        let result = Recorder::start(&region, &settings).map(|r| (r, region));
        post_boxed(send.get(), WM_APP_STARTED, result);
    });
}

fn on_started(hwnd: HWND, result: Result<(Recorder, CaptureRegion), String>) {
    match result {
        Ok((rec, region)) => {
            app(|a| {
                a.recorder = Some(rec);
                a.state = State::Recording;
            });
            frame::show(hwnd, &region, None);
            set_tip(hwnd, "Clipikon: recording");
            update_tray_icon(hwnd);
            unsafe {
                SetTimer(Some(hwnd), FAIL_TIMER, 500, None);
            }
        }
        Err(e) => {
            app(|a| a.state = State::Idle);
            message_box(&e, true);
        }
    }
}

fn stop_recording(hwnd: HWND) {
    let Some(rec) = app(|a| {
        a.state = State::Stopping;
        a.recorder.take()
    }) else {
        return;
    };
    unsafe {
        let _ = KillTimer(Some(hwnd), FAIL_TIMER);
    }
    frame::close();
    set_tip(hwnd, "Clipikon: saving…");
    update_tray_icon(hwnd); // back to idle: state is Stopping now
    let send = SendHwnd(hwnd.0 as isize);
    std::thread::spawn(move || post_boxed(send.get(), WM_APP_STOPPED, rec.stop()));
}

fn on_stopped(hwnd: HWND, result: Result<PathBuf, String>) {
    let (open, quit) = app(|a| {
        a.state = State::Idle;
        (a.settings.open_editor_after_recording, a.quit_after_stop)
    });
    if quit {
        unsafe { PostQuitMessage(0) };
    }
    set_tip(hwnd, "Clipikon");
    match result {
        Ok(file) if open => open_editor(&file),
        Ok(file) => notify(hwnd, &format!("Saved {}", file.file_name().unwrap_or_default().to_string_lossy())),
        Err(e) => message_box(&e, true),
    }
}

fn check_failure(hwnd: HWND) {
    let failed = app(|a| a.recorder.as_ref().filter(|r| r.has_failed()).map(|r| r.error_text()));
    if let Some(err) = failed {
        stop_recording(hwnd); // salvage what was recorded
        message_box(&format!("Recording stopped unexpectedly:\n{err}"), true);
    }
}

/// HWND is a raw pointer (not Send); posting to it from another thread is fine.
#[derive(Clone, Copy)]
struct SendHwnd(isize);
impl SendHwnd {
    fn get(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => on_hotkey_down(hwnd),
        WM_APP_STOP => match app(|a| a.state) {
            State::Recording => stop_recording(hwnd),
            State::Countdown => cancel_countdown(hwnd),
            _ => {}
        },
        WM_APP_TRAY => match lp.0 as u32 {
            WM_LBUTTONUP => on_hotkey(hwnd),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        },
        WM_APP_SELECTED => on_selected(hwnd, unsafe { take_boxed(lp) }),
        WM_APP_STARTED => on_started(hwnd, unsafe { take_boxed(lp) }),
        WM_APP_STOPPED => on_stopped(hwnd, unsafe { take_boxed(lp) }),
        // Light/dark taskbar switches arrive as a setting change.
        WM_SETTINGCHANGE => update_tray_icon(hwnd),
        WM_APP_QUICK_DONE => on_quick_done(hwnd, unsafe { take_boxed(lp) }),
        WM_TIMER if wp.0 == FAIL_TIMER => check_failure(hwnd),
        WM_TIMER if wp.0 == HOLD_TIMER => check_hold(hwnd),
        WM_TIMER if wp.0 == COUNTDOWN_TIMER => countdown_done(hwnd),
        m if m == APP.with(|a| a.borrow().as_ref().map(|a| a.taskbar_created).unwrap_or(u32::MAX)) => add_tray_icon(hwnd),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse_hotkey("Alt+Shift+R").unwrap(), (MOD_ALT | MOD_SHIFT, 'R' as u32));
        assert_eq!(parse_hotkey("ctrl + f9").unwrap(), (MOD_CONTROL, VK_F9.0 as u32));
        assert_eq!(parse_hotkey("Win+PrintScreen").unwrap().1, VK_SNAPSHOT.0 as u32);
        assert!(parse_hotkey("Alt+Shift").is_err());
        assert!(parse_hotkey("Alt+Banana").is_err());
    }
}
