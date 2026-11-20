//! Small Win32 helpers shared by the tray daemon's windows.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_SELECTED: u32 = WM_APP + 2;
pub const WM_APP_STARTED: u32 = WM_APP + 3;
pub const WM_APP_STOPPED: u32 = WM_APP + 4;
pub const WM_APP_STOP: u32 = WM_APP + 5;
pub const WM_APP_QUICK_DONE: u32 = WM_APP + 6;

pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

pub fn register_class(name: PCWSTR, proc: WndProc, cursor: PCWSTR) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

pub fn create_window(ex: WINDOW_EX_STYLE, class: PCWSTR, style: WINDOW_STYLE, r: RECT) -> HWND {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(ex, class, w!("Clipikon"), style, r.left, r.top, r.right - r.left, r.bottom - r.top, None, None, Some(instance.into()), None)
            .unwrap_or_default()
    }
}

pub fn message_box(text: &str, error: bool) {
    let t = wide(text);
    let flags = if error { MB_OK | MB_ICONERROR } else { MB_OK | MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), w!("Clipikon"), flags | MB_SETFOREGROUND | MB_TOPMOST);
    }
}

/// Scale factor (1.0 = 96 DPI) of the monitor containing the point.
pub fn scale_at(x: i32, y: i32) -> f64 {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        dx as f64 / 96.0
    }
}

pub fn fill(hdc: HDC, r: RECT, color: COLORREF) {
    unsafe {
        let b = CreateSolidBrush(color);
        FillRect(hdc, &r, b);
        let _ = DeleteObject(b.into());
    }
}

pub fn font(px: i32, bold: bool) -> HFONT {
    font_face(px, bold, "Segoe UI")
}

pub fn font_face(px: i32, bold: bool, face: &str) -> HFONT {
    let face = wide(face);
    unsafe {
        CreateFontW(-px, 0, 0, 0, if bold { 600 } else { 400 }, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, PCWSTR(face.as_ptr()))
    }
}

/// The system icon font: Segoe Fluent Icons on Windows 11, Segoe MDL2 Assets (same code points) on 10.
pub fn icon_face() -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()));
        if dir.join("Fonts").join("SegoeIcons.ttf").is_file() { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }
    })
}

pub fn draw_text(hdc: HDC, text: &str, r: &mut RECT, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
    let mut t: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, color);
        DrawTextW(hdc, &mut t, r, flags);
    }
}

/// Measures text with the currently selected font.
pub fn text_size(hdc: HDC, text: &str) -> (i32, i32) {
    let mut t: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT::default();
    unsafe {
        DrawTextW(hdc, &mut t, &mut r, DT_CALCRECT | DT_SINGLELINE);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// Double-buffered WM_PAINT: `draw` renders into an offscreen DC of the client size.
pub fn paint(hwnd: HWND, draw: impl FnOnce(HDC, RECT)) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem, bmp.into());
        draw(mem, rc);
        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}
