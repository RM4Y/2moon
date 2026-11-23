#!/usr/bin/env bash
# Build FFmpeg (H.264, HEVC, AV1 via VideoToolbox + dav1d) and Opus for macOS.
#   scripts/build-ffmpeg-macos.sh -> target/ffmpeg-bundle/macos-<arch>/{lib,LICENSES}
# Needs the Xcode tools; same pinned sources as build-ffmpeg.sh.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VERSION=8.1.3
FFMPEG_SHA256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
DAV1D_VERSION=1.5.5
DAV1D_SHA256=ca96b1d0ba3f01f55de7231d775892b64994ba061aaeeaaa4f662a390b3abf9e
OPUS_VERSION=1.6.1
OPUS_SHA256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1

ARCH="$(uname -m)"
SRC="$ROOT/target/ffmpeg-src"
OUT="$ROOT/target/ffmpeg-bundle/macos-$ARCH"
STAMP="$OUT/.built-ffmpeg-$FFMPEG_VERSION-dav1d-$DAV1D_VERSION-opus-$OPUS_VERSION-vt-enc"
[ -f "$STAMP" ] && { echo "$OUT (up to date)"; exit 0; }

mkdir -p "$SRC"
fetch() { # url file sha256
    [ -f "$SRC/$2" ] || { curl -sSfL "$1" -o "$SRC/$2.part" && mv "$SRC/$2.part" "$SRC/$2"; }
    echo "$3  $SRC/$2" | shasum -a 256 -c --quiet - || { echo "checksum mismatch for $2" >&2; rm -f "$SRC/$2"; exit 1; }
}
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
fetch "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" "dav1d-$DAV1D_VERSION.tar.xz" "$DAV1D_SHA256"
fetch "https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz" "opus-$OPUS_VERSION.tar.gz" "$OPUS_SHA256"

# Build tools for dav1d, kept inside target/.
VENV="$ROOT/target/macos-buildtools"
[ -x "$VENV/bin/meson" ] || { python3 -m venv "$VENV" && "$VENV/bin/pip" install -q meson ninja pkgconf; }
export PATH="$VENV/bin:$PATH"
command -v pkg-config >/dev/null || ln -sf "$VENV/bin/pkgconf" "$VENV/bin/pkg-config"

B="$ROOT/target/ffmpeg-build-macos-$ARCH"
P="$B/prefix"
rm -rf "$B" "$OUT"
mkdir -p "$B" "$P" "$OUT/lib" "$OUT/LICENSES"
export MACOSX_DEPLOYMENT_TARGET=12.0

tar -C "$B" -xJf "$SRC/dav1d-$DAV1D_VERSION.tar.xz"
meson setup "$B/dav1d-$DAV1D_VERSION" "$B/dav1d-build" --prefix="$P" --libdir=lib --buildtype=release \
    -Ddefault_library=static -Denable_tools=false -Denable_tests=false -Denable_examples=false >/dev/null
ninja -C "$B/dav1d-build" install >/dev/null

# Opus (audio of the stream, microphone): libopus.0.dylib, loaded at run time.
tar -C "$B" -xzf "$SRC/opus-$OPUS_VERSION.tar.gz"
meson setup "$B/opus-$OPUS_VERSION" "$B/opus-build" --prefix="$B/opus-prefix" --libdir=lib --buildtype=release \
    -Ddefault_library=shared -Dtests=disabled -Ddocs=disabled -Dextra-programs=disabled >/dev/null
ninja -C "$B/opus-build" install >/dev/null
cp "$B"/opus-prefix/lib/libopus.0.dylib "$OUT/lib/"
install_name_tool -id @loader_path/libopus.0.dylib "$OUT/lib/libopus.0.dylib"
cp "$B/opus-$OPUS_VERSION/COPYING" "$OUT/LICENSES/Opus-BSD-3-Clause.txt"

tar -C "$B" -xJf "$SRC/ffmpeg-$FFMPEG_VERSION.tar.xz"
cd "$B/ffmpeg-$FFMPEG_VERSION"
PKG_CONFIG_PATH="$P/lib/pkgconfig" ./configure --prefix="$P" \
    --enable-shared --disable-static --enable-pic \
    --install-name-dir=@loader_path \
    --disable-programs --disable-doc --disable-network --disable-autodetect \
    --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample \
    --disable-everything \
    --enable-decoder=h264,hevc,av1,libdav1d --enable-parser=h264,hevc,av1 \
    --enable-libdav1d \
    --enable-videotoolbox --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox,av1_videotoolbox \
    --enable-encoder=h264_videotoolbox,hevc_videotoolbox \
    --pkg-config-flags=--static >/dev/null
make -j"$(sysctl -n hw.ncpu)" >/dev/null
make install >/dev/null

cp -a "$P"/lib/libavcodec*.dylib "$P"/lib/libavutil*.dylib "$OUT/lib/"
strip -x "$OUT"/lib/*.*.*.dylib "$OUT/lib/libopus.0.dylib"
# The configure line kept in the libraries names the build folder: replace it (same length).
for f in "$OUT"/lib/*.*.*.dylib; do
    PFX="$P" perl -pi -e 'BEGIN { $o = $ENV{PFX}; $n = substr("/build/ffmpeg-prefix" . ("/" x length($o)), 0, length($o)) } s/\Q$o\E/$n/g' "$f"
    codesign -f -s - "$f" 2>/dev/null
done
cp COPYING.LGPLv2.1 "$OUT/LICENSES/FFmpeg-LGPL-2.1.txt"
cp "$B/dav1d-$DAV1D_VERSION/COPYING" "$OUT/LICENSES/dav1d-BSD-2-Clause.txt"
printf "FFmpeg %s (LGPL 2.1, https://ffmpeg.org) with dav1d %s (BSD 2-Clause, https://code.videolan.org/videolan/dav1d), and Opus %s (BSD 3-Clause, https://opus-codec.org).\nBuilt by scripts/build-ffmpeg-macos.sh from the unmodified release tarballs; decoders: h264, hevc, av1, libdav1d; hwaccel: VideoToolbox; encoders (host): h264 / hevc VideoToolbox.\n" "$FFMPEG_VERSION" "$DAV1D_VERSION" "$OPUS_VERSION" > "$OUT/LICENSES/README.txt"
touch "$STAMP"
ls -la "$OUT/lib"
otool -L "$OUT"/lib/libavcodec.*.*.*.dylib
