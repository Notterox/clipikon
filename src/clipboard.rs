//! Puts a file (CF_HDROP) or an image (CF_DIB + PNG) on the clipboard, so it can be pasted into chat apps,
//! image editors or Explorer.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
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

/// Copies an RGBA image as a bitmap (what most apps paste) and as PNG (what browsers and some chat apps prefer).
pub fn copy_image(width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    if rgba.len() != w * h * 4 {
        return Err("bad image size".into());
    }
    let mut items = Vec::new();

    // CF_DIB: header + bottom-up BGRA rows.
    let header = size_of::<BITMAPINFOHEADER>();
    let dib = global(header + w * h * 4, |buf| {
        let bih = BITMAPINFOHEADER {
            biSize: header as u32,
            biWidth: width as i32,
            biHeight: height as i32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (w * h * 4) as u32,
            ..Default::default()
        };
        unsafe { std::ptr::write_unaligned(buf.as_mut_ptr() as *mut BITMAPINFOHEADER, bih) };
        for y in 0..h {
            let src = &rgba[(h - 1 - y) * w * 4..(h - y) * w * 4];
            let dst = &mut buf[header + y * w * 4..header + (y + 1) * w * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        }
    })?;
    items.push((CF_DIB.0 as u32, dib));

    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_compression(png::Compression::Fast);
        let written = enc.write_header().and_then(|mut wr| wr.write_image_data(rgba));
        if written.is_err() {
            png_bytes.clear();
        }
    }
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if !png_bytes.is_empty() && png_format != 0 {
        if let Ok(mem) = global(png_bytes.len(), |buf| buf.copy_from_slice(&png_bytes)) {
            items.push((png_format, mem));
        }
    }
    set(items)
}
