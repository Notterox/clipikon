//! Puts a file (CF_HDROP) or an image (PNG + CF_DIB) on the clipboard, so it can be pasted into chat apps,
//! image editors or Explorer.

use crate::screenshot::Image;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardOwner, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP};
use windows::Win32::UI::Shell::DROPFILES;
use windows::core::w;

/// Global memory holding `fill`'s bytes, for SetClipboardData.
fn global(len: usize, fill: impl FnOnce(&mut [u8])) -> Result<HGLOBAL, String> {
    unsafe {
        let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, len).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err("GlobalLock failed".into());
        }
        fill(std::slice::from_raw_parts_mut(ptr, len));
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// Replaces the clipboard contents with `items` (format, memory). Memory the clipboard didn't take is freed.
fn set(items: Vec<(u32, HGLOBAL)>) -> Result<(), String> {
    unsafe {
        if let Err(e) = OpenClipboard(None) {
            for (_, mem) in items {
                let _ = GlobalFree(Some(mem));
            }
            return Err(e.to_string());
        }
        let _ = EmptyClipboard();
        let mut result = Ok(());
        for (format, mem) in items {
            if let Err(e) = SetClipboardData(format, Some(HANDLE(mem.0))) {
                let _ = GlobalFree(Some(mem)); // ownership only passes to the clipboard on success
                result = Err(e.to_string());
            }
        }
        let _ = CloseClipboard();
        result
    }
}

pub fn copy_file(path: &Path) -> Result<(), String> {
    // DROPFILES header followed by a double-NUL-terminated list of wide paths.
    let mut list: Vec<u16> = path.as_os_str().encode_wide().collect();
    list.extend([0, 0]);
    let header = size_of::<DROPFILES>();
    let mem = global(header + list.len() * 2, |buf| {
        let df = DROPFILES { pFiles: header as u32, fWide: true.into(), ..Default::default() };
        unsafe { std::ptr::write_unaligned(buf.as_mut_ptr() as *mut DROPFILES, df) };
        for (i, c) in list.iter().enumerate() {
            buf[header + i * 2..header + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
    })?;
    set(vec![(CF_HDROP.0 as u32, mem)])
}

/// Copies an RGBA image (an editor frame) as a compact PNG and a 24-bit bitmap.
pub fn copy_image(width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if rgba.len() != width as usize * height as usize * 4 {
        return Err("bad image size".into());
    }
    let png = crate::screenshot::encode_png(width, height, rgba, crate::screenshot::Order::Rgba)?;
    let bgra = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    let dib = dib(&Image { width, height, bgra })?;
    let mut items = vec![(CF_DIB.0 as u32, dib)];
    if let Ok(mem) = global(png.len(), |buf| buf.copy_from_slice(&png)) {
        items.push((png_format(), mem));
    }
    set(items)
}

/// Copies a PNG as is: the clipboard holds only the compressed image. The bitmap that most apps paste
/// (CF_DIB) is promised but only made when an app asks for it, so `owner` must answer WM_RENDERFORMAT with
/// `render_dib` and WM_RENDERALLFORMATS with `render_all`.
pub fn copy_png(owner: HWND, png: &[u8]) -> Result<(), String> {
    let mem = global(png.len(), |buf| buf.copy_from_slice(png))?;
    unsafe {
        if let Err(e) = OpenClipboard(Some(owner)) {
            let _ = GlobalFree(Some(mem));
            return Err(e.to_string());
        }
        let _ = EmptyClipboard();
        let result = SetClipboardData(png_format(), Some(HANDLE(mem.0))).map(drop).map_err(|e| e.to_string());
        if result.is_err() {
            let _ = GlobalFree(Some(mem));
        }
        let _ = SetClipboardData(CF_DIB.0 as u32, None); // delayed rendering
        let _ = CloseClipboard();
        result
    }
}

/// WM_RENDERFORMAT for CF_DIB: puts the bitmap of `png` on the clipboard, which the pasting app has open.
pub fn render_dib(png: &[u8]) {
    let Ok(img) = crate::screenshot::decode_png(png) else { return };
    if let Ok(mem) = dib(&img) {
        unsafe {
            if SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(mem.0))).is_err() {
                let _ = GlobalFree(Some(mem));
            }
        }
    }
}

/// WM_RENDERALLFORMATS (the owner is quitting): makes the promised bitmap now, if the clipboard is still ours.
pub fn render_all(owner: HWND, png: &[u8]) {
    unsafe {
        if OpenClipboard(Some(owner)).is_ok() {
            if GetClipboardOwner().ok() == Some(owner) {
                render_dib(png);
            }
            let _ = CloseClipboard();
        }
    }
}

fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

/// CF_DIB memory: a header and bottom-up 24-bit rows, each padded to 4 bytes (a quarter smaller than 32-bit).
fn dib(img: &Image) -> Result<HGLOBAL, String> {
    let (w, h) = (img.width as usize, img.height as usize);
    let stride = (w * 3).div_ceil(4) * 4;
    let header = size_of::<BITMAPINFOHEADER>();
    global(header + stride * h, |buf| {
        let bih = BITMAPINFOHEADER {
            biSize: header as u32,
            biWidth: w as i32,
            biHeight: h as i32,
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            biSizeImage: (stride * h) as u32,
            ..Default::default()
        };
        unsafe { std::ptr::write_unaligned(buf.as_mut_ptr() as *mut BITMAPINFOHEADER, bih) };
        for y in 0..h {
            let src = &img.bgra[(h - 1 - y) * w * 4..(h - y) * w * 4];
            let dst = &mut buf[header + y * stride..header + y * stride + w * 3];
            for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                d.copy_from_slice(&s[..3]);
            }
        }
    })
}
