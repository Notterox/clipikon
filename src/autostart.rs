//! "Launch at startup": the `Clipikon` value under HKCU\...\Run. The installer's checkbox writes the same
//! value, so the tray menu shows and changes either.

use crate::win::wide;
use windows::Win32::System::Registry::*;
use windows::core::{PCWSTR, w};

const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// Where Task Manager's Startup apps page records that the user disabled a Run entry.
const APPROVED: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const NAME: PCWSTR = w!("Clipikon");

pub fn enabled() -> bool {
    registered().is_some() && !disabled_in_task_manager()
}

pub fn set(on: bool) {
    unsafe {
        if on {
            write(&this_exe());
            // Turning it on here also undoes a "Disabled" set in Task Manager.
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED, NAME);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME);
        }
    }
}

/// Points an entry whose exe no longer exists (a moved portable copy) at this exe. An entry for another copy
/// that still exists, such as the installed one, is left alone.
pub fn repair() {
    if let Some(cmd) = registered()
        && !std::path::Path::new(cmd.trim_matches('"')).is_file()
    {
        unsafe { write(&this_exe()) };
    }
}

fn this_exe() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    format!("\"{}\"", exe.display())
}

unsafe fn write(cmd: &str) {
    let data = wide(cmd);
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN, NAME, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32);
    }
}

/// The registered command line, if there is one.
fn registered() -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN, NAME, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    // `size` counts bytes, including the terminating null.
    r.is_ok().then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
}

/// Task Manager stores 12 bytes per entry; an odd first byte means disabled (2 = enabled, 3 = disabled).
fn disabled_in_task_manager() -> bool {
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, APPROVED, NAME, RRF_RT_REG_BINARY, None, Some(data.as_mut_ptr().cast()), Some(&mut size))
    };
    r.is_ok() && size > 0 && data[0] & 1 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changes the real HKCU Run key, so it only runs on request: `cargo test -- --ignored autostart`.
    #[test]
    #[ignore]
    fn toggles_and_repairs() {
        let before = registered();
        set(true);
        assert!(enabled());
        assert_eq!(registered(), Some(this_exe()));
        unsafe { write(r#""C:\nowhere\clipikon.exe""#) };
        repair();
        assert_eq!(registered(), Some(this_exe()));
        set(false);
        assert!(!enabled());
        if let Some(cmd) = before {
            unsafe { write(&cmd) };
        }
    }
}
