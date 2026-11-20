//! Embeds the app icon, generated from `src/icon_art.rs`. Statically links the minimal FFmpeg from
//! `ffmpeg-build/out` (committed prebuilt; `ffmpeg-build/build.cmd` rebuilds it) when it exists; clipikon.exe
//! then doubles as ffmpeg.exe. Without it the app still builds and looks for an ffmpeg.exe next to itself or
//! on PATH.

use std::path::PathBuf;

#[allow(dead_code)] // the app uses the rest
#[path = "src/icon_art.rs"]
mod icon_art;

fn main() {
    embed_icon();
    println!("cargo::rustc-check-cfg=cfg(linked_ffmpeg)");
    println!("cargo::rerun-if-env-changed=CLIPIKON_FFMPEG_DIR");
    let dir = std::env::var_os("CLIPIKON_FFMPEG_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("ffmpeg-build").join("out")
    });
    let tools = dir.join("fftools.lib");
    // The whole folder, so a rebuilt avfilter.lib etc. relinks too.
    println!("cargo::rerun-if-changed={}", dir.display());
    if !tools.is_file() {
        println!("cargo::warning=No {} — building without FFmpeg linked in (restore ffmpeg-build/out from git, or run ffmpeg-build/build.cmd).", tools.display());
        return;
    }

    println!("cargo::rustc-link-search=native={}", dir.display());
    // fftools first: it pulls in the rest. MSVC's linker resolves across libraries in any order anyway.
    for lib in ["fftools", "avfilter", "avformat", "avcodec", "swresample", "swscale", "avutil"] {
        println!("cargo::rustc-link-lib=static={lib}");
    }
    let syslibs = std::fs::read_to_string(dir.join("syslibs.txt")).unwrap_or_default();
    for lib in syslibs.split_whitespace() {
        println!("cargo::rustc-link-lib=dylib={lib}");
    }
    let version = std::fs::read_to_string(dir.join("FFmpeg-VERSION.txt")).unwrap_or_default();
    println!("cargo::rustc-cfg=linked_ffmpeg");
    println!("cargo::rustc-env=CLIPIKON_FFMPEG_VERSION={}", version.trim());
}

/// The exe's icon (Explorer, taskbar, Alt+Tab): every size drawn by `icon_art`, as resource 1.
fn embed_icon() {
    println!("cargo::rerun-if-changed=src/icon_art.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let ico = icon_art::ico(&[16, 20, 24, 32, 40, 48, 64, 256], icon_art::Style::Color, |size, rgba| {
        let mut png_bytes = Vec::new();
        let mut enc = png::Encoder::new(&mut png_bytes, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().and_then(|mut w| w.write_image_data(rgba)).expect("png");
        png_bytes
    });
    let ico_path = out.join("clipikon.ico");
    std::fs::write(&ico_path, ico).unwrap();
    let rc = out.join("clipikon.rc");
    // rc string literals need doubled backslashes.
    let escaped = ico_path.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{escaped}\"\n")).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().unwrap();
}
