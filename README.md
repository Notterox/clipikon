# Clipikon

https://github.com/user-attachments/assets/57140a14-2d9e-4873-a5ad-8f6543032b7d

Clipikon is a screen recording tool. Press Alt+Shift+R, drag over an area and record, then trim, crop
and export the clip as MP4, WebM or GIF. Recording runs on the GPU, so it barely affects games.

Hotkeys and settings are in the [guide](docs/guide.md).

## Platform

Windows 10 or 11 with an NVIDIA GPU (driver 531.61 or newer). Clipikon comes as an installer or as a single
exe that runs without one.

Download the installer or the zip from [Releases](https://github.com/Notterox/clipikon/releases). Windows may
warn that the app is unrecognized, because the exe isn't signed; choose More info, then Run anyway.

## Build

You need Rust with the MSVC toolchain, and Visual Studio or its Build Tools with the C++ workload.

```bat
cargo build --release
```

## Is it built with AI?

Yes, with Claude.

## License

Clipikon is released under the [MIT License](LICENSE). It contains code under other licenses:

- FFmpeg 9.0.2 (LGPL 2.1 or later), statically linked and built from the official source by
  `ffmpeg-build/build.sh`. It includes SVT-AV1 (BSD-3-Clause-Clear) and Opus (BSD-3-Clause). Their license
  texts are in `ffmpeg-build/out/FFmpeg-LICENSE.txt`.
- The FSRCNNX upscaling shader by igv (LGPL 3.0 or later), in `src/shaders`. The license text is in
  `licenses/LGPL-3.0.txt`.
