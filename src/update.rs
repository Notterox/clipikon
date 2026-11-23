//! "Check for updates": compares this build's version with the latest GitHub release and, if that one is newer,
//! downloads it. An installed copy (the installer's uninstaller sits next to the exe) runs the new installer
//! silently; a portable copy swaps its exe for the one in the release zip. Either way Clipikon then restarts.

use crate::win::wide;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::Win32::Networking::WinHttp::*;
use windows::core::{PCWSTR, w};

const LATEST: &str = "https://api.github.com/repos/Notterox/clipikon/releases/latest";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Release {
    pub version: String,
    asset: Asset,
}

struct Asset {
    name: String,
    url: String,
    size: u64,
}

/// A downloaded update, ready to hand over to.
pub enum Ready {
    /// The new installer, to run silently.
    Installer(PathBuf),
    /// The exe is already replaced: start it.
    Portable(PathBuf),
}

/// The latest release, if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    check_against(CURRENT, is_installed())
}

fn check_against(current: &str, installed: bool) -> Result<Option<Release>, String> {
    let body = get(LATEST, "application/vnd.github+json")?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("Unexpected reply from GitHub: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("Unexpected reply from GitHub.")?;
    let version = tag.trim_start_matches('v').to_string();
    if !is_newer(&version, current) {
        return Ok(None);
    }
    let wanted = |name: &str| {
        if installed { name.starts_with("Clipikon-Setup-") && name.ends_with(".exe") } else { name.ends_with("-windows-x64.zip") }
    };
    let asset = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(wanted))
        .and_then(|a| {
            Some(Asset {
                name: a["name"].as_str()?.to_string(),
                url: a["browser_download_url"].as_str()?.to_string(),
                size: a["size"].as_u64()?,
            })
        })
        .ok_or_else(|| format!("Release {tag} has no {} to download.", if installed { "installer" } else { "zip" }))?;
    Ok(Some(Release { version, asset }))
}

/// Downloads the release. A portable copy also replaces its exe here, keeping the old one as `<exe>.old` until
/// the next start.
pub fn download(release: &Release) -> Result<Ready, String> {
    let dir = update_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let file = fetch(&release.asset, &dir)?;
    if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return Ok(Ready::Installer(file));
    }
    let new = unpack(&file, &dir.join("unpacked"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_exe(&exe);
    let _ = std::fs::remove_file(&old);
    // A running exe can be renamed, not overwritten.
    std::fs::rename(&exe, &old).map_err(|e| format!("Couldn't replace {}: {e}", exe.display()))?;
    if let Err(e) = std::fs::copy(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Couldn't replace {}: {e}", exe.display()));
    }
    Ok(Ready::Portable(exe))
}

/// Starts the installer or the new exe. The caller then quits.
pub fn launch(ready: &Ready) -> std::io::Result<()> {
    match ready {
        // The installer closes open editor windows and starts Clipikon again when it's done.
        Ready::Installer(setup) => Command::new(setup).args(["/SILENT", "/CLOSEAPPLICATIONS", "/NORESTART"]).spawn(),
        Ready::Portable(exe) => Command::new(exe).spawn(),
    }
    .map(drop)
}

/// After an update: removes the previous exe and the download. The old process may still be exiting, so this
/// retries for a while in the background.
pub fn clean_up() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = old_exe(&exe);
    std::thread::spawn(move || {
        for _ in 0..20 {
            if !old.exists() || std::fs::remove_file(&old).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = std::fs::remove_dir_all(update_dir());
    });
}

/// The installer puts its uninstaller next to the exe; a portable copy has none.
fn is_installed() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.with_file_name("unins000.exe").is_file())
}

fn old_exe(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

fn update_dir() -> PathBuf {
    std::env::temp_dir().join("Clipikon").join("update")
}

fn fetch(asset: &Asset, dir: &Path) -> Result<PathBuf, String> {
    let data = get(&asset.url, "application/octet-stream")?;
    if data.len() as u64 != asset.size {
        return Err("The download was incomplete. Try again later.".into());
    }
    let name = Path::new(&asset.name).file_name().ok_or("Bad file name in the release.")?;
    let file = dir.join(name);
    std::fs::write(&file, data).map_err(|e| format!("Couldn't save {}: {e}", file.display()))?;
    Ok(file)
}

/// Extracts the zip with the tar.exe that comes with Windows and returns the clipikon.exe inside.
fn unpack(zip: &Path, out: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let tar = std::env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("tar.exe")).unwrap_or("tar.exe".into());
    let status = Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(out)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("Couldn't run tar.exe to unpack the update: {e}"))?;
    if !status.success() {
        return Err("Couldn't unpack the update.".into());
    }
    find_file(out, "clipikon.exe", 3).ok_or_else(|| "The update has no clipikon.exe.".into())
}

fn find_file(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    if let Some(f) = entries.iter().find(|p| p.is_file() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name))) {
        return Some(f.clone());
    }
    (depth > 0).then(|| entries.iter().filter(|p| p.is_dir()).find_map(|d| find_file(d, name, depth - 1))).flatten()
}

/// "1.10.0" > "1.9.2"; a missing part counts as older ("0.2" < "0.2.0").
fn is_newer(latest: &str, current: &str) -> bool {
    let parts = |v: &str| v.split(['.', '-', '+']).map_while(|p| p.parse::<u64>().ok()).collect::<Vec<_>>();
    parts(latest) > parts(current)
}

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut core::ffi::c_void);

impl Handle {
    fn new(h: *mut core::ffi::c_void) -> Result<Self, String> {
        if h.is_null() { Err(std::io::Error::last_os_error().to_string()) } else { Ok(Self(h)) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// An HTTPS GET with WinHTTP (the system's proxy settings and certificates), following redirects.
fn get(url: &str, accept: &str) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("Only https URLs are supported.")?;
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let net = |e: windows::core::Error| format!("Network error: {}", e.message());
    let agent = wide(&format!("Clipikon/{CURRENT}"));
    let (host, path) = (wide(host), wide(&path));
    let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
    unsafe {
        let session = Handle::new(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0))?;
        let connection = Handle::new(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(net)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(net)?;
        let (mut status, mut len) = (0u32, 4u32);
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(net)?;
        if status != 200 {
            return Err(format!("GitHub replied with HTTP {status}."));
        }
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read).map_err(net)?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.10.0", "1.9.2"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    /// Talks to GitHub, so it only runs on request: `cargo test -- --ignored update`.
    #[test]
    #[ignore]
    fn finds_and_unpacks_the_latest_release() {
        assert!(check_against(CURRENT, false).unwrap().is_none(), "this build is the latest release");
        let installer = check_against("0.0.0", true).unwrap().expect("a newer release");
        assert!(installer.asset.name.starts_with("Clipikon-Setup-"));
        let release = check_against("0.0.0", false).unwrap().expect("a newer release");
        let dir = std::env::temp_dir().join("Clipikon").join("update-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip = fetch(&release.asset, &dir).unwrap();
        let exe = unpack(&zip, &dir.join("unpacked")).unwrap();
        assert!(std::fs::metadata(&exe).unwrap().len() > 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
