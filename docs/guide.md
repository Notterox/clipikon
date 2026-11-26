# Clipikon guide

Snipping-Tool-style screen recorder for gameplay on Windows + NVIDIA. Press a hotkey, drag a region, and
only that region is recorded at 60+ fps (or your monitor's refresh rate). Then trim, crop and compress for
sharing.

## How it works

- **Capture → encode stays on the GPU.** FFmpeg's `ddagrab` (Desktop Duplication) crops the region on the
  GPU and hands D3D11 frames straight to NVENC (`h264_nvenc`), so game performance is barely touched.
- **System audio** is captured with WASAPI loopback (cpal) and aligned to the video by timestamps when
  you stop.
- **Editor** (`clipikon edit <file>`, Windows 11 Fluent style with Mica) gives you a preview, trim handles,
  a crop tool (free or 16:9, 9:16, 1:1, 4:5), and these export options. The preview is decoded at the size
  it's shown; when the view is larger than the video, it's upscaled with the method picked next to Copy
  frame: **FSRCNNX** (default; a small neural network that doubles the picture, from 1.3× up), **FSR 1**
  (AMD's EASU + RCAS shaders) or plain **Bilinear**. Export options:
  - formats: MP4, MKV, WebM or GIF
  - fast lossless cut (MP4/MKV)
  - compress to a target size (10/25/50/100 MB, lowering the resolution automatically if needed)
  - compress by quality (the default, Balanced), as H.264 or HEVC (NVENC) or AV1 (SVT-AV1 on the CPU: smallest files, slower;
    WebM is always AV1 + Opus)
  - GIF: frame rate, playback speed (0.5–2×), width, colors (32–256), dithering, loop, per-frame palette.
    **Preview GIF** renders it once and plays the real result with its exact size; Export then just saves it

  **Copy file** puts the result on the clipboard to paste into Discord etc. The image button next to the
  time (or Ctrl+C) copies the current frame as a picture, cropped and at full resolution.

## Use

| | |
|---|---|
| `Alt+Shift+R` | Opens the picker. Step 1: drag an area (a click selects the whole screen), or switch the toolbar to **Window** (click a window) or **Display**. Step 2: drag the handles to resize, inside to move, outside to start over; set audio, cursor, frame rate and start delay on the toolbar (for this recording only); **Record** or Enter starts. Esc or right-click cancels. Press the hotkey again to stop recording. The microphone button is a placeholder for now. |
| Hold `Alt+Shift+R` | After ~0.4 s a quick settings flyout opens: frame rate (up to the monitor's refresh rate), start delay (3/5/10 s), audio, cursor, and Record (Enter, or press the hotkey again). Choices there apply to that recording only and aren't saved. Esc or clicking away closes it. |
| `Alt+Shift+S` | Screenshot. The screen freezes and the picker opens on it: drag an area (a click takes the whole screen), or switch to **Window** or **Display** and click one. It's taken as soon as you pick. The toolbar's cursor and delay options apply to this screenshot only; with a delay the area is taken live once the countdown ends. The camera and video buttons on the toolbar switch between screenshot and recording. |
| Tray icon | Left-click = record. Right-click = menu (record, screenshot, open video, folder, capture options, settings, launch at startup, check for updates). Options changed here are saved. |
| Editor | Space play/pause · I/O set start/end · ←/→ frame step (Shift: 1 s) · C crop (Enter done, Esc cancel) · Ctrl+C copy frame |

With a start delay, the frame and a countdown pill appear after you select the region; its Cancel button (or
the hotkey) cancels.

Screenshots are copied to the clipboard and saved as PNG in `Clipikon` inside Windows' Screenshots folder (usually `Pictures\Screenshots`); click the
notification to edit one (or turn on **Edit screenshots right away** in the tray menu). The editor crops,
draws (pen, highlighter, arrow, rectangle, text), pixelates private details and erases marks; the Select
tool moves a mark, changes its color or size, or deletes it (Delete), and a double-click edits text. Ctrl+Z /
Ctrl+Y undo and redo, Ctrl+C copies, Ctrl+S saves over the file, Ctrl+Shift+S saves as PNG or JPEG. A PNG
dropped on `clipikon.exe` opens in it too.

Click the selected tool again for its options (color, thickness, smoothing, fill). The round color button
sets any color, for the drawing tool or the selected mark; with the eyedropper beside it, a click on the
image copies that color's hex code and Shift+click draws in it. The mouse wheel zooms toward the pointer; drag with the middle button or Space held to pan; Ctrl+0
fits the window and Ctrl+1 shows actual size. The gear button sets the size of copies and saves (10% to 400%; an upscale is
either Sharp, smooth with crisp edges, or Pixels, each pixel a block), and the format: PNG (lossless, or 256 colors:
usually less than half the size) or JPEG (with its quality). While it's open the image shows the result and
its file size. Save as starts with that format; Copy is always PNG and Save keeps the file's format. The editor remembers its tools and these settings. The PNG is lossless but small: an image with few colors (most UI) is stored with a
palette. The clipboard holds only the PNG, and makes a bitmap when an app that can't paste PNG asks for one.

Settings: `%APPDATA%\Clipikon\settings.json` (hotkeys, fps where 0 = refresh rate, start delay, quality,
output folder, audio offset, and for screenshots: folder, cursor, delay). Recordings go to `Clipikon` in the Videos library's save location. Left unset (`null`), both folders
follow Windows if you move those folders.

## Build

FFmpeg is statically linked into `clipikon.exe`, so the result is a single file with nothing to install.
The app starts itself as its own FFmpeg worker process. Building needs:

- Rust (MSVC toolchain)
- Visual Studio or its Build Tools, with the C++ workload (for the linker)

```bat
cargo build --release
```

The result is `target\release\clipikon.exe`. The FFmpeg libraries it links come prebuilt in `ffmpeg-build/out`
(about 43 MB).

### Rebuilding FFmpeg

`ffmpeg-build/build.sh` produced those libraries: a minimal LGPL FFmpeg with only what Clipikon uses (ddagrab,
NVENC/NVDEC, SVT-AV1, Opus, GIF, MP4/MKV/WebM, and a few filters), plus `fftools.lib`, the ffmpeg command-line
tool with `main()` renamed so `clipikon.exe` can act as `ffmpeg.exe`. To rebuild them, for another FFmpeg
version or other options, you also need [w64devkit](https://github.com/skeeto/w64devkit) for `sh`, `make` and
`nasm`, and Visual Studio's CMake and Ninja (part of the C++ workload). It takes about 10 minutes and downloads
the FFmpeg, nv-codec-headers, SVT-AV1 and Opus sources:

```bat
set W64DEVKIT=C:\path\to\w64devkit
ffmpeg-build\build.cmd
cargo build --release
```

Without `ffmpeg-build/out`, `cargo build` still works. The app then uses an `ffmpeg.exe` (7.0+ with ddagrab
and NVENC) from next to `clipikon.exe` or from PATH.

An `ffmpeg.exe` placed next to `clipikon.exe` always takes priority over the built-in one.

## License notes

The built-in FFmpeg is LGPL v2.1+; SVT-AV1 (BSD-3-Clause-Clear plus the AOMedia patent license) and Opus
(BSD-3-Clause) are linked into it. When distributing `clipikon.exe`:

- **Source:** publish Clipikon's source, including `ffmpeg-build/`. Static linking is allowed under the LGPL
  as long as users can relink with a modified FFmpeg, and the source plus build script lets them.
- **License texts:** include `ffmpeg-build/out/FFmpeg-LICENSE.txt`, which holds FFmpeg's, SVT-AV1's and
  Opus's licenses. The tray menu's **About Clipikon** shows it too.
- **Source link:** point to the FFmpeg version's source (https://ffmpeg.org/releases/).

Everything enabled is LGPL-compatible: no `--enable-gpl` or `--enable-nonfree`. NVENC/NVDEC use the NVIDIA
driver at runtime.

The editor's FSRCNNX upscaler is `src/shaders/FSRCNNX_x2_8-0-4-1.glsl` from
[igv/FSRCNN-TensorFlow](https://github.com/igv/FSRCNN-TensorFlow) (release 1.1), unmodified and licensed
under the LGPL v3 or later. It's compiled from that file at runtime, so publishing the source covers it;
include the LGPL v3 text (and the GPL v3 it builds on) with releases.
