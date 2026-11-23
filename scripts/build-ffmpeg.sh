#!/usr/bin/env bash
# Build the Linux FFmpeg: decoders (with VAAPI) and host encoders (VAAPI, NVENC),
# in a Debian 12 container.
#   scripts/build-ffmpeg.sh -> target/ffmpeg-bundle/linux-x86_64/{lib,LICENSES}
# Sources are checked against the pinned SHA-256 sums below (FFmpeg: also its signature,
# key FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VERSION=8.1.3
FFMPEG_SHA256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
DAV1D_VERSION=1.5.5
DAV1D_SHA256=ca96b1d0ba3f01f55de7231d775892b64994ba061aaeeaaa4f662a390b3abf9e
NVHDR_VERSION=12.1.14.1
NVHDR_SHA256=043e46265517855f13bcb98b3a225b2761b2a83983b1645fa1d05876f1c3a781

SRC="$ROOT/target/ffmpeg-src"
OUT="$ROOT/target/ffmpeg-bundle/linux-x86_64"
STAMP="$OUT/.built-ffmpeg-$FFMPEG_VERSION-dav1d-$DAV1D_VERSION-nv-$NVHDR_VERSION-enc"
[ -f "$STAMP" ] && { echo "$OUT (up to date)"; exit 0; }

mkdir -p "$SRC"
fetch() { # url file sha256
    [ -f "$SRC/$2" ] || curl -sSfL "$1" -o "$SRC/$2.part" && { [ -f "$SRC/$2" ] || mv "$SRC/$2.part" "$SRC/$2"; }
    echo "$3  $SRC/$2" | sha256sum -c --quiet - || { echo "checksum mismatch for $2" >&2; rm -f "$SRC/$2"; exit 1; }
}
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
fetch "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" "dav1d-$DAV1D_VERSION.tar.xz" "$DAV1D_SHA256"
fetch "https://github.com/FFmpeg/nv-codec-headers/releases/download/n$NVHDR_VERSION/nv-codec-headers-$NVHDR_VERSION.tar.gz" "nv-codec-headers-$NVHDR_VERSION.tar.gz" "$NVHDR_SHA256"

rm -rf "$OUT"
mkdir -p "$OUT"
docker run --rm -v "$SRC":/src:ro -v "$OUT":/out -e FFV="$FFMPEG_VERSION" -e DAV="$DAV1D_VERSION" -e NVV="$NVHDR_VERSION" -e OWNER="$(id -u):$(id -g)" \
    rust:1.97-bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends nasm meson ninja-build pkg-config libva-dev patchelf >/dev/null
    B=/tmp/build; P=/tmp/prefix; mkdir -p $B $P
    tar -C $B -xJf /src/dav1d-$DAV.tar.xz
    meson setup $B/dav1d-$DAV $B/dav1d-build --prefix=$P --libdir=lib --buildtype=release \
        -Ddefault_library=static -Denable_tools=false -Denable_tests=false -Denable_examples=false -Db_pie=true >/dev/null
    ninja -C $B/dav1d-build install >/dev/null
    tar -C $B -xzf /src/nv-codec-headers-$NVV.tar.gz
    make -C $B/nv-codec-headers-$NVV install PREFIX=$P >/dev/null
    tar -C $B -xJf /src/ffmpeg-$FFV.tar.xz
    cd $B/ffmpeg-$FFV
    PKG_CONFIG_PATH=$P/lib/pkgconfig ./configure --prefix=$P \
        --enable-shared --disable-static --enable-pic \
        --disable-programs --disable-doc --disable-network --disable-autodetect \
        --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample \
        --disable-everything \
        --enable-decoder=h264,hevc,av1,libdav1d --enable-parser=h264,hevc,av1 \
        --enable-libdav1d --enable-vaapi --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi \
        --enable-ffnvcodec --enable-nvenc \
        --enable-encoder=h264_vaapi,hevc_vaapi,av1_vaapi,h264_nvenc,hevc_nvenc,av1_nvenc \
        --pkg-config-flags=--static \
        --extra-ldflags="-Wl,--as-needed" >/dev/null
    make -j"$(nproc)" >/dev/null
    make install >/dev/null
    mkdir -p /out/lib /out/LICENSES
    cp -a $P/lib/libavcodec.so.* $P/lib/libavutil.so.* /out/lib/
    strip --strip-unneeded /out/lib/*.so.*.*
    # libavcodec finds the bundled libavutil next to it.
    for l in /out/lib/*.so.*.*; do patchelf --set-rpath "\$ORIGIN" "$l"; done
    cp COPYING.LGPLv2.1 /out/LICENSES/FFmpeg-LGPL-2.1.txt
    cp $B/dav1d-$DAV/COPYING /out/LICENSES/dav1d-BSD-2-Clause.txt
    printf "FFmpeg %s (LGPL 2.1, https://ffmpeg.org) with dav1d %s (BSD 2-Clause, https://code.videolan.org/videolan/dav1d).\nBuilt by scripts/build-ffmpeg.sh from the unmodified release tarballs; decoders: h264, hevc, av1, libdav1d; hwaccel: VAAPI; encoders: h264/hevc/av1 vaapi and nvenc (nv-codec-headers %s, MIT).\n" $FFV $DAV $NVV > /out/LICENSES/README.txt
    chown -R $OWNER /out
'
touch "$STAMP"
ls -la "$OUT/lib"
