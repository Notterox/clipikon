//! Per-pixel-alpha drawing for small Win32 overlay windows (hint, size label, recording pill): a 32-bit
//! premultiplied DIB that is pushed to a layered window with UpdateLayeredWindow. Unlike a color-keyed window
//! with one constant alpha, this gives opaque, crisp text and anti-aliased rounded corners.

use crate::win::*;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

/// Premultiplied BGRA pixel, as `0xAARRGGBB`.
pub fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    let p = |c: u8| (c as u32 * a as u32 + 127) / 255;
    (a as u32) << 24 | p(r) << 16 | p(g) << 8 | p(b)
}

pub struct Canvas {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Option<Self> {
        let (w, h) = (w.max(1), h.max(1));
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Self { dc, bmp, old, bits: bits as *mut u32, w, h })
        }
    }

    fn px(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }

    pub fn clear(&mut self) {
        self.px().fill(0);
    }

    /// Replaces the pixels of `r` with `color`.
    pub fn fill(&mut self, r: RECT, color: u32) {
        let w = self.w;
        let (l, t, rr, b) = (r.left.max(0), r.top.max(0), r.right.min(self.w), r.bottom.min(self.h));
        if l >= rr {
            return;
        }
        let px = self.px();
        for y in t..b {
            px[(y * w + l) as usize..(y * w + rr) as usize].fill(color);
        }
    }

    /// A rounded panel: 1px `border` around a `fill`, the Fluent flyout look.
    pub fn panel(&mut self, r: RECT, radius: f32, fill: u32, border: u32) {
        self.round_rect(r, radius, border);
        let inner = RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 };
        self.round_rect(inner, (radius - 1.0).max(0.0), fill);
    }

    /// Anti-aliased rounded rectangle, blended over what's there.
    pub fn round_rect(&mut self, r: RECT, radius: f32, color: u32) {
        let (w, h) = (self.w, self.h);
        let (l, t, rr, b) = (r.left.max(0), r.top.max(0), r.right.min(w), r.bottom.min(h));
        let rad = radius.min((r.right - r.left) as f32 / 2.0).min((r.bottom - r.top) as f32 / 2.0);
        let px = self.px();
        for y in t..b {
            for x in l..rr {
                // Distance from the pixel center into the nearest corner's circle, if in a corner square.
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let cx = fx.clamp(r.left as f32 + rad, r.right as f32 - rad);
                let cy = fy.clamp(r.top as f32 + rad, r.bottom as f32 - rad);
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                let cov = (rad + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let i = (y * w + x) as usize;
                    px[i] = blend(color, px[i], cov);
                }
            }
        }
    }

    /// Anti-aliased filled circle.
    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: u32) {
        let d = radius.ceil() as i32 + 1;
        let r = RECT { left: cx as i32 - d, top: cy as i32 - d, right: cx as i32 + d + 1, bottom: cy as i32 + d + 1 };
        let (w, h) = (self.w, self.h);
        let px = self.px();
        for y in r.top.max(0)..r.bottom.min(h) {
            for x in r.left.max(0)..r.right.min(w) {
                let dist = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let cov = (radius + 0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let i = (y * w + x) as usize;
                    px[i] = blend(color, px[i], cov);
                }
            }
        }
    }

    pub fn text_size(&self, text: &str, px: i32, bold: bool) -> (i32, i32) {
        unsafe {
            let f = font(px, bold);
            let old = SelectObject(self.dc, f.into());
            let size = text_size(self.dc, text);
            SelectObject(self.dc, old);
            let _ = DeleteObject(f.into());
            size
        }
    }

    /// GDI text, which must sit on an opaque area (GDI clears the alpha of what it draws; it's restored here).
    pub fn text(&mut self, r: RECT, text: &str, px: i32, bold: bool, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
        self.text_face(r, text, px, bold, "Segoe UI", color, flags);
    }

    /// A Segoe Fluent Icons glyph, centered in `r` (which must be on an opaque area, as for `text`).
    pub fn glyph(&mut self, r: RECT, glyph: &str, px: i32, color: COLORREF) {
        self.text_face(r, glyph, px, false, icon_face(), color, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_face(&mut self, r: RECT, text: &str, px: i32, bold: bool, face: &str, color: COLORREF, flags: DRAW_TEXT_FORMAT) {
        unsafe {
            let f = font_face(px, bold, face);
            let old = SelectObject(self.dc, f.into());
            let mut rc = r;
            draw_text(self.dc, text, &mut rc, color, flags | DT_NOPREFIX);
            SelectObject(self.dc, old);
            let _ = DeleteObject(f.into());
            let _ = GdiFlush();
        }
        let w = self.w;
        let (l, t, rr, b) = (r.left.max(0), r.top.max(0), r.right.min(self.w), r.bottom.min(self.h));
        let px = self.px();
        for y in t..b {
            for p in &mut px[(y * w + l) as usize..(y * w + rr) as usize] {
                *p |= 0xFF00_0000;
            }
        }
    }

    /// Shows the canvas as `hwnd`'s content, with its top-left at `pos` (screen coordinates).
    pub fn present(&self, hwnd: HWND, pos: POINT) {
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let size = SIZE { cx: self.w, cy: self.h };
        unsafe {
            let _ = UpdateLayeredWindow(hwnd, None, Some(&pos), Some(&size), Some(self.dc), Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// `src` over `dst` (both premultiplied), with `src` scaled by coverage.
fn blend(src: u32, dst: u32, cov: f32) -> u32 {
    let ch = |v: u32, s: u32| (v >> s) & 0xFF;
    let sa = ch(src, 24) as f32 * cov / 255.0;
    let mut out = 0u32;
    for s in [0, 8, 16, 24] {
        let v = ch(src, s) as f32 * cov + ch(dst, s) as f32 * (1.0 - sa);
        out |= (v.round().clamp(0.0, 255.0) as u32) << s;
    }
    out
}
