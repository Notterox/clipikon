//! Screenshots: grabbing the screen with GDI (every monitor, whichever GPU drives it), the mouse cursor, and a
//! compact lossless PNG.
//!
//! The picker freezes the screen when it opens: `grab` copies each display into a `Frame` (a DIB section the
//! overlay paints from), and the chosen area is cut out of that.

use crate::displays::Display;
use std::collections::HashMap;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CopyIcon, DI_NORMAL, DestroyIcon, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};

/// Pixels as top-down BGRA rows; the alpha byte is unused (screenshots are opaque).
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// A 32-bit DIB section selected into its own memory DC: GDI draws into it and its pixels can be read.
pub struct Frame {
    pub dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pub width: i32,
    pub height: i32,
}

impl Frame {
    pub fn new(width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            let old = SelectObject(dc, bitmap.into());
            Some(Self { dc, bitmap, old, bits: bits.cast(), width, height })
        }
    }

    fn len(&self) -> usize {
        (self.width * self.height * 4) as usize
    }

    pub fn pixels(&self) -> &[u8] {
        unsafe {
            let _ = GdiFlush(); // finish pending GDI drawing before reading the bits
            std::slice::from_raw_parts(self.bits, self.len())
        }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, self.len())
        }
    }

    /// The `r` part of this frame (in its pixels) as a new frame.
    pub fn crop(&self, r: RECT) -> Option<Frame> {
        let f = Frame::new(r.right - r.left, r.bottom - r.top)?;
        unsafe {
            BitBlt(f.dc, 0, 0, f.width, f.height, Some(self.dc), r.left, r.top, SRCCOPY).ok()?;
        }
        Some(f)
    }

    /// A darker copy: each channel scaled by `keep`/255.
    pub fn dimmed(&self, keep: u8) -> Option<Frame> {
        let mut f = Frame::new(self.width, self.height)?;
        let table: [u8; 256] = std::array::from_fn(|v| (v * keep as usize / 255) as u8);
        for (d, s) in f.pixels_mut().iter_mut().zip(self.pixels()) {
            *d = table[*s as usize];
        }
        Some(f)
    }

    pub fn to_image(&self) -> Image {
        Image { width: self.width as u32, height: self.height as u32, bgra: self.pixels().to_vec() }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The mouse cursor as it was when grabbed: its image and where its top-left corner was on screen.
pub struct Cursor {
    icon: HICON,
    x: i32,
    y: i32,
}

impl Cursor {
    pub fn grab() -> Option<Cursor> {
        unsafe {
            let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
            GetCursorInfo(&mut info).ok()?;
            if info.flags.0 & CURSOR_SHOWING.0 == 0 {
                return None;
            }
            let icon = CopyIcon(HICON(info.hCursor.0)).ok()?;
            let mut ii = ICONINFO::default();
            if GetIconInfo(icon, &mut ii).is_err() {
                let _ = DestroyIcon(icon);
                return None;
            }
            for b in [ii.hbmMask, ii.hbmColor] {
                if !b.is_invalid() {
                    let _ = DeleteObject(b.into());
                }
            }
            let (x, y) = (info.ptScreenPos.x - ii.xHotspot as i32, info.ptScreenPos.y - ii.yHotspot as i32);
            Some(Cursor { icon, x, y })
        }
    }

    /// Draws the cursor onto `frame`, whose top-left pixel is at screen point `origin`.
    pub fn draw(&self, frame: &Frame, origin: POINT) {
        unsafe {
            let _ = DrawIconEx(frame.dc, self.x - origin.x, self.y - origin.y, self.icon, 0, 0, 0, None, DI_NORMAL);
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.icon);
        }
    }
}

/// The screen as it is now: one frame per display, and the cursor.
pub struct Grab {
    pub frames: Vec<Frame>,
    pub cursor: Option<Cursor>,
}

pub fn grab(displays: &[Display]) -> Option<Grab> {
    let cursor = Cursor::grab();
    let frames = displays
        .iter()
        .map(|d| grab_rect(RECT { left: d.left, top: d.top, right: d.left + d.width, bottom: d.top + d.height }))
        .collect::<Option<Vec<_>>>()?;
    Some(Grab { frames, cursor })
}

/// Copies screen rectangle `r` (virtual-desktop pixels).
pub fn grab_rect(r: RECT) -> Option<Frame> {
    let f = Frame::new(r.right - r.left, r.bottom - r.top)?;
    unsafe {
        let screen = GetDC(None);
        // CAPTUREBLT includes layered windows (tooltips, menus with shadows, our own pills aside: those are
        // excluded from capture).
        let ok = BitBlt(f.dc, 0, 0, f.width, f.height, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT);
        ReleaseDC(None, screen);
        ok.ok()?;
    }
    Some(f)
}

/// Channel order of 4-byte pixels given to `encode_png`.
#[derive(Clone, Copy, PartialEq)]
pub enum Order {
    Bgra,
    Rgba,
}

/// Encodes opaque 4-byte pixels as a small lossless PNG: a 1/2/4/8-bit palette when there are at most 256
/// colors (common for UI), else 24-bit RGB with per-row filters, both with the strongest deflate setting.
pub fn encode_png(width: u32, height: u32, px: &[u8], order: Order) -> Result<Vec<u8>, String> {
    let n = width as usize * height as usize;
    if n == 0 || px.len() < n * 4 {
        return Err("bad image size".into());
    }
    let rgb = |i: usize| {
        let p = &px[i * 4..i * 4 + 3];
        if order == Order::Bgra { [p[2], p[1], p[0]] } else { [p[0], p[1], p[2]] }
    };
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_compression(png::Compression::Best);
    let data = match palette(n, rgb) {
        Some((colors, indices)) => {
            let bits = match colors.len() {
                0..=2 => 1,
                3..=4 => 2,
                5..=16 => 4,
                _ => 8,
            };
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(match bits {
                1 => png::BitDepth::One,
                2 => png::BitDepth::Two,
                4 => png::BitDepth::Four,
                _ => png::BitDepth::Eight,
            });
            enc.set_palette(colors.concat());
            // Filters only help on continuous tones; on palette indices they cost size.
            enc.set_filter(png::FilterType::NoFilter);
            pack(&indices, width as usize, bits)
        }
        None => {
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
            (0..n).flat_map(rgb).collect()
        }
    };
    enc.write_header().and_then(|mut w| w.write_image_data(&data)).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The image's colors and each pixel's index into them, if there are at most 256.
fn palette(n: usize, rgb: impl Fn(usize) -> [u8; 3]) -> Option<(Vec<[u8; 3]>, Vec<u8>)> {
    let mut map: HashMap<[u8; 3], u8> = HashMap::new();
    let mut colors = Vec::new();
    let mut indices = Vec::with_capacity(n);
    let mut last: Option<([u8; 3], u8)> = None;
    for i in 0..n {
        let c = rgb(i);
        let k = match last {
            Some((lc, lk)) if lc == c => lk, // runs of one color are the norm on screens
            _ => {
                let k = match map.get(&c) {
                    Some(&k) => k,
                    None if colors.len() == 256 => return None,
                    None => {
                        let k = colors.len() as u8;
                        map.insert(c, k);
                        colors.push(c);
                        k
                    }
                };
                last = Some((c, k));
                k
            }
        };
        indices.push(k);
    }
    Some((colors, indices))
}

/// Packs `bits`-wide indices into bytes, each row starting on a byte, leftmost pixel in the high bits.
fn pack(indices: &[u8], width: usize, bits: usize) -> Vec<u8> {
    if bits == 8 {
        return indices.to_vec();
    }
    let row_bytes = (width * bits).div_ceil(8);
    let rows = indices.len() / width;
    let mut out = vec![0u8; row_bytes * rows];
    for (y, row) in indices.chunks_exact(width).enumerate() {
        for (x, &k) in row.iter().enumerate() {
            let bit = x * bits;
            out[y * row_bytes + bit / 8] |= k << (8 - bits - bit % 8);
        }
    }
    out
}

/// Decodes a PNG made by `encode_png` (or any 8-bit RGB/RGBA/palette PNG) to top-down BGRA.
pub fn decode_png(data: &[u8]) -> Result<Image, String> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let n = (info.width * info.height) as usize;
    let bgra = match info.color_type {
        png::ColorType::Rgb => buf[..n * 3].chunks_exact(3).flat_map(|p| [p[2], p[1], p[0], 255]).collect(),
        png::ColorType::Rgba => buf[..n * 4].chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect(),
        png::ColorType::Grayscale => buf[..n].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..n * 2].chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], 255]).collect(),
        png::ColorType::Indexed => return Err("palette not expanded".into()),
    };
    Ok(Image { width: info.width, height: info.height, bgra })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
        let png = encode_png(w, h, bgra, Order::Bgra).unwrap();
        let back = decode_png(&png).unwrap();
        assert_eq!((back.width, back.height), (w, h));
        let strip = |v: &[u8]| v.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<_>>();
        assert_eq!(strip(&back.bgra), strip(bgra));
        png
    }

    #[test]
    fn palette_and_rgb_pngs_are_lossless() {
        // 3 colors, odd width: a 2-bit palette with padded rows.
        let few: Vec<u8> = (0..7 * 5).flat_map(|i| [[10, 20, 30, 0], [200, 0, 0, 0], [0, 0, 255, 0]][i % 3]).collect();
        let small = roundtrip(7, 5, &few);
        // A gradient with more than 256 colors: 24-bit RGB.
        let many: Vec<u8> = (0..64 * 64u32).flat_map(|i| [(i % 64) as u8 * 4, (i / 64) as u8 * 4, (i % 7) as u8, 0]).collect();
        roundtrip(64, 64, &many);
        assert!(small.len() < 120, "{} bytes", small.len());
    }

    #[test]
    fn packs_sub_byte_indices() {
        assert_eq!(pack(&[1, 0, 1, 1, 0, 0, 0, 0, 1], 9, 1), vec![0b1011_0000, 0b1000_0000]);
        assert_eq!(pack(&[3, 2, 1], 3, 2), vec![0b1110_0100]);
    }

    /// Grabs the real screen and reports PNG sizes and times: `cargo test --release -- --ignored --nocapture png_size`.
    #[test]
    #[ignore]
    fn png_size_of_a_real_screenshot() {
        let d = crate::displays::enumerate().into_iter().next().unwrap();
        let f = grab_rect(RECT { left: d.left, top: d.top, right: d.left + d.width, bottom: d.top + d.height }).unwrap();
        let img = f.to_image();
        let t = std::time::Instant::now();
        let png = encode_png(img.width, img.height, &img.bgra, Order::Bgra).unwrap();
        let enc = t.elapsed();
        let mut fast = Vec::new();
        let mut e = png::Encoder::new(&mut fast, img.width, img.height);
        e.set_color(png::ColorType::Rgba);
        e.set_compression(png::Compression::Fast);
        let rgba: Vec<u8> = img.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        e.write_header().unwrap().write_image_data(&rgba).unwrap();
        let t = std::time::Instant::now();
        decode_png(&png).unwrap();
        println!(
            "{}x{}: compact {} KB in {enc:?} (decode {:?}), old fast RGBA {} KB, raw DIB {} KB",
            img.width,
            img.height,
            png.len() / 1024,
            t.elapsed(),
            fast.len() / 1024,
            img.bgra.len() / 1024
        );
    }
}
