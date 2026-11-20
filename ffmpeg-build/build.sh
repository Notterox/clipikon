#!/bin/sh
# Builds the minimal LGPL FFmpeg that is statically linked into clipikon.exe. Run it through build.cmd, which
# sets up the Visual Studio compiler (and its CMake/Ninja); this script itself needs a POSIX shell, make and
# nasm (w64devkit).
#
# Output (ffmpeg-build/out):
#   *.lib          FFmpeg libraries, plus fftools.lib: the ffmpeg command-line tool with main() renamed to
#                  ffmpeg_main(), so clipikon.exe can act as ffmpeg.exe
#   syslibs.txt    other libraries they need (Windows ones, SvtAv1Enc, opus)
#
# Only what Clipikon uses is enabled: ddagrab capture, NVENC/NVDEC, H.264/HEVC/AAC/Opus in MP4/MKV, AV1 (SVT-AV1,
# on the CPU, since NVENC AV1 needs an RTX 40) + Opus for WebM, GIF, and the handful of filters for cropping,
# trimming, scaling, GIF palettes and audio alignment. No GPL or nonfree parts.
set -e

FFMPEG_VERSION=9.0.2
# NVENC API 12.1 works with NVIDIA drivers 531.61 and newer.
NVHEADERS_VERSION=n12.1.14.0
SVTAV1_VERSION=v3.1.2
OPUS_VERSION=1.5.2

here=$(cd "$(dirname "$0")" && pwd)
work=$here/work
prefix=$work/prefix
out=$here/out
mkdir -p "$work/tmp" "$prefix" "$out"
cd "$work"
# configure can't use a Windows-style %TEMP% (backslashes).
export TMPDIR="$work/tmp"

command -v cl >/dev/null || { echo "cl.exe not found: run this through build.cmd"; exit 1; }

fetch() {
    [ -f "$2" ] && return
    if command -v curl >/dev/null 2>&1; then curl -fsSL -o "$2.part" "$1"; else wget -q -O "$2.part" "$1"; fi
    mv "$2.part" "$2"
}

fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION.tar.xz"
fetch "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/$NVHEADERS_VERSION.tar.gz" "nv-codec-headers-$NVHEADERS_VERSION.tar.gz"
[ -d "ffmpeg-$FFMPEG_VERSION" ] || tar xf "ffmpeg-$FFMPEG_VERSION.tar.xz"
fetch "https://gitlab.com/AOMediaCodec/SVT-AV1/-/archive/$SVTAV1_VERSION/SVT-AV1-$SVTAV1_VERSION.tar.gz" "SVT-AV1-$SVTAV1_VERSION.tar.gz"
fetch "https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz" "opus-$OPUS_VERSION.tar.gz"
[ -d "nv-codec-headers-$NVHEADERS_VERSION" ] || tar xzf "nv-codec-headers-$NVHEADERS_VERSION.tar.gz"
[ -d "SVT-AV1-$SVTAV1_VERSION" ] || tar xzf "SVT-AV1-$SVTAV1_VERSION.tar.gz"
[ -d "opus-$OPUS_VERSION" ] || tar xzf "opus-$OPUS_VERSION.tar.gz"
# busybox awk mis-parses `n = -v ? a : b` (n gets -v), which garbles config.mak; parenthesize the ternary.
sed -i 's/ = -v ? \(.*\);$/ = (-v ? \1);/' "ffmpeg-$FFMPEG_VERSION/configure"

# nv-codec-headers is header-only; install it by hand rather than via its Makefile.
mkdir -p "$prefix/include" "$prefix/lib/pkgconfig"
cp -r "nv-codec-headers-$NVHEADERS_VERSION/include/ffnvcodec" "$prefix/include/"
sed -e "s#@@PREFIX@@#$prefix#" -e "s#@@VERSION@@#${NVHEADERS_VERSION#n}#" \
    "nv-codec-headers-$NVHEADERS_VERSION/ffnvcodec.pc.in" > "$prefix/lib/pkgconfig/ffnvcodec.pc"
export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"

# Encoder libraries built with CMake: static, static C runtime, installed into $prefix with .pc files.
cmake_lib() { # <source dir> <build dir> <extra cmake args...>
    srcdir=$1 bdir=$2
    shift 2
    [ -f "$bdir/.installed" ] && return
    cmake -S "$srcdir" -B "$bdir" -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix" \
        -DCMAKE_C_COMPILER=cl -DCMAKE_CXX_COMPILER=cl -DCMAKE_POLICY_DEFAULT_CMP0091=NEW \
        -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DBUILD_SHARED_LIBS=OFF "$@"
    cmake --build "$bdir"
    cmake --install "$bdir"
    touch "$bdir/.installed"
}
cmake_lib "SVT-AV1-$SVTAV1_VERSION" build-svtav1 -DBUILD_APPS=OFF -DBUILD_DEC=OFF -DBUILD_TESTING=OFF -DCMAKE_ASM_NASM_COMPILER=nasm
cmake_lib "opus-$OPUS_VERSION" build-opus -DOPUS_BUILD_PROGRAMS=OFF -DOPUS_BUILD_TESTING=OFF

# Reconfigure from scratch whenever this script changes: with MSVC, make's dependency tracking misses the
# regenerated component lists, so an incremental build would keep stale filter/codec tables.
if [ ! -f build/config.h ] || [ "$here/build.sh" -nt build/config.h ]; then
    rm -rf build
fi
mkdir -p build
cd build
if [ ! -f config.h ]; then
    # -MT: static C runtime, matching clipikon.exe (+crt-static).
    "../ffmpeg-$FFMPEG_VERSION/configure" \
        --toolchain=msvc --arch=x86_64 --target-os=win64 \
        --prefix="$prefix" --pkg-config=pkg-config --extra-cflags="-MT -I$prefix/include" \
        --disable-everything --disable-autodetect --disable-debug --disable-doc --disable-network \
        --disable-ffplay --disable-ffprobe --disable-avdevice \
        --enable-static --disable-shared --enable-w32threads \
        --enable-d3d11va --enable-ffnvcodec --enable-nvenc --enable-nvdec --enable-cuda \
        --enable-libsvtav1 --enable-libopus \
        --enable-filter=ddagrab,setparams,crop,scale,fps,trim,atrim,setpts,asetpts,adelay,apad,aresample,aformat,format,null,anull,hwdownload,split,palettegen,paletteuse \
        --enable-encoder=h264_nvenc,hevc_nvenc,libsvtav1,aac,libopus,gif,rawvideo,pcm_s16le \
        --enable-decoder=h264,hevc,aac,opus,mp3float,flac,pcm_s16le,pcm_s24le,pcm_f32le \
        --enable-hwaccel=h264_nvdec,hevc_nvdec \
        --enable-demuxer=mov,matroska,wav,mp3 \
        --enable-muxer=mp4,mov,matroska,webm,gif,rawvideo,null,wav \
        --enable-parser=h264,hevc,av1,aac,opus,mpegaudio,flac \
        --enable-bsf=aac_adtstoasc,extract_extradata,h264_mp4toannexb,hevc_mp4toannexb,null \
        --enable-protocol=file,pipe
fi
make -j"$(nproc)"
make install

# The ffmpeg tool as a library: its objects, with ffmpeg.o swapped for a copy whose main() is ffmpeg_main().
# (Renaming in the source: -Dmain=... doesn't work, cmdutils.h does #undef main.)
src=../ffmpeg-$FFMPEG_VERSION/fftools
sed 's/^int main(int argc, char \*\*argv)$/int ffmpeg_main(int argc, char **argv)/' "$src/ffmpeg.c" > "$src/ffmpeg_lib.c"
grep -q '^int ffmpeg_main(' "$src/ffmpeg_lib.c" || { echo "couldn't rename main() in ffmpeg.c"; exit 1; }
make fftools/ffmpeg_lib.o
printf 'print-objs:\n\t@echo $(OBJS-ffmpeg)\n' > print-objs.mak
objs=$(make -s -f Makefile -f print-objs.mak print-objs | sed 's#fftools/ffmpeg\.o#fftools/ffmpeg_lib.o#')
rm -f "$out"/*.lib
lib -nologo -out:"$out/fftools.lib" $objs
for l in avfilter avformat avcodec swresample swscale avutil SvtAv1Enc opus; do
    cp "$prefix/lib/$l.lib" "$out/$l.lib"
done

# Windows libraries the libraries and the ffmpeg tool link against (e.g. "ole32.lib user32.lib").
sed -n -e 's/^EXTRALIBS-\(avfilter\|avformat\|avcodec\|swresample\|swscale\|avutil\|ffmpeg\)=//p' ffbuild/config.mak \
    | tr ' ' '\n' | sed -n 's/\.lib$//p' | sort -u > "$out/syslibs.txt"
{
    cat "../ffmpeg-$FFMPEG_VERSION/COPYING.LGPLv2.1"
    printf '\n\n==== SVT-AV1 %s (https://gitlab.com/AOMediaCodec/SVT-AV1) ====\n\n' "$SVTAV1_VERSION"
    cat "../SVT-AV1-$SVTAV1_VERSION/LICENSE.md" "../SVT-AV1-$SVTAV1_VERSION/PATENTS.md"
    printf '\n\n==== Opus %s (https://opus-codec.org) ====\n\n' "$OPUS_VERSION"
    cat "../opus-$OPUS_VERSION/COPYING"
} > "$out/FFmpeg-LICENSE.txt"
echo "$FFMPEG_VERSION" > "$out/FFmpeg-VERSION.txt"
ls -l "$out"
cat "$out/syslibs.txt"
