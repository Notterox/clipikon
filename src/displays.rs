use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

/// A monitor as Desktop Duplication sees it. Bounds are physical virtual-desktop pixels.
#[derive(Clone, Debug)]
pub struct Display {
    /// ddagrab's `output_idx`.
    pub output_index: u32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    /// Current refresh rate in Hz (0 if unknown).
    pub refresh_hz: u32,
}

impl Display {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }
}

/// A capture region in the display's local physical pixels.
#[derive(Clone, Debug)]
pub struct CaptureRegion {
    pub display: Display,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl CaptureRegion {
    pub fn screen_x(&self) -> i32 {
        self.display.left + self.x
    }
    pub fn screen_y(&self) -> i32 {
        self.display.top + self.y
    }

    pub fn full(display: &Display) -> Self {
        Self { display: display.clone(), x: 0, y: 0, width: display.width & !1, height: display.height & !1 }
    }

    /// Builds a region from two drag corners (display-local). A click without drag means the whole display.
    pub fn from_drag(d: &Display, x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        // NVENC rejects very small frames; tiny selections are grown to this.
        const MIN_W: i32 = 160;
        const MIN_H: i32 = 96;
        let (mut x, mut y) = (x0.min(x1), y0.min(y1));
        let (mut w, mut h) = ((x1 - x0).abs(), (y1 - y0).abs());
        if w < 8 && h < 8 {
            return Self::full(d);
        }
        if w < MIN_W {
            x -= (MIN_W - w) / 2;
            w = MIN_W;
        }
        if h < MIN_H {
            y -= (MIN_H - h) / 2;
            h = MIN_H;
        }
        w = w.min(d.width) & !1; // encoders want even dimensions
        h = h.min(d.height) & !1;
        Self { display: d.clone(), x: x.clamp(0, d.width - w), y: y.clamp(0, d.height - h), width: w, height: h }
    }
}

/// Outputs of DXGI adapter 0, in the order ddagrab numbers them. ddagrab creates its D3D11 device on the
/// default adapter, so only those outputs are capturable (with one discrete GPU that's every monitor).
pub fn enumerate() -> Vec<Display> {
    let mut list = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return list };
        let Ok(adapter) = factory.EnumAdapters1(0) else { return list };
        let mut i = 0;
        while let Ok(output) = adapter.EnumOutputs(i) {
            if let Ok(d) = output.GetDesc() {
                if d.AttachedToDesktop.as_bool() {
                    let r = d.DesktopCoordinates;
                    list.push(Display {
                        output_index: i,
                        left: r.left,
                        top: r.top,
                        width: r.right - r.left,
                        height: r.bottom - r.top,
                        refresh_hz: refresh_rate(&d.DeviceName),
                    });
                }
            }
            i += 1;
        }
    }
    list
}

/// The display mode's refresh rate for a GDI device name like `\\.\DISPLAY1`.
fn refresh_rate(device: &[u16; 32]) -> u32 {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(windows::core::PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    // 0 and 1 mean "hardware default".
    if ok.as_bool() && mode.dmDisplayFrequency > 1 { mode.dmDisplayFrequency } else { 0 }
}

/// The display containing the cursor (for menus that show its refresh rate).
pub fn under_cursor() -> Option<Display> {
    let mut p = windows::Win32::Foundation::POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    let list = enumerate();
    list.iter().find(|d| d.contains(p.x, p.y)).or(list.first()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> Display {
        Display { output_index: 0, left: 1920, top: 0, width: 2560, height: 1440, refresh_hz: 165 }
    }

    #[test]
    fn click_selects_whole_display() {
        let r = CaptureRegion::from_drag(&display(), 100, 100, 103, 102);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 2560, 1440));
    }

    #[test]
    fn drag_normalizes_and_evens() {
        let r = CaptureRegion::from_drag(&display(), 901, 701, 100, 100);
        assert_eq!((r.x, r.y, r.width, r.height), (100, 100, 800, 600));
        assert_eq!((r.screen_x(), r.screen_y()), (2020, 100));
    }

    #[test]
    fn tiny_drag_grows_and_stays_on_screen() {
        let r = CaptureRegion::from_drag(&display(), 2550, 1435, 2530, 1420);
        assert_eq!((r.width, r.height), (160, 96));
        assert!(r.x + r.width <= 2560 && r.y + r.height <= 1440);
    }
}
