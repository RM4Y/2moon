#!/usr/bin/env bash
# Build the Windows DLLs (FFmpeg, Opus, SDL3) with MinGW in a Debian 12 container.
#   scripts/build-windows-deps.sh -> target/windows-deps/{*.dll,LICENSES}
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VERSION=8.1.3
FFMPEG_SHA256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
DAV1D_VERSION=1.5.5
DAV1D_SHA256=ca96b1d0ba3f01f55de7231d775892b64994ba061aaeeaaa4f662a390b3abf9e
OPUS_VERSION=1.6.1
OPUS_SHA256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1
SDL_VERSION=3.4.20
SDL_SHA256=f3bb06f55bed9557227a700af2ea62e49497ebdf58037732a731a5fb3ef8581f
# Encoder headers / dispatcher (MIT). NVENC API 12.1: NVIDIA driver 531 or newer.
NVHDR_VERSION=12.1.14.1
NVHDR_SHA256=043e46265517855f13bcb98b3a225b2761b2a83983b1645fa1d05876f1c3a781
AMF_VERSION=1.4.36
AMF_SHA256=ec07ee21b820a73f5bae224e92fd3c492f52732d4885f0637f48189afdd87564
VPL_VERSION=2.16.0
VPL_SHA256=d60931937426130ddad9f1975c010543f0da99e67edb1c6070656b7947f633b6

SRC="$ROOT/target/ffmpeg-src"
OUT="$ROOT/target/windows-deps"
STAMP="$OUT/.built-ffmpeg-$FFMPEG_VERSION-dav1d-$DAV1D_VERSION-opus-$OPUS_VERSION-sdl-$SDL_VERSION-nv-$NVHDR_VERSION-amf-$AMF_VERSION-vpl-$VPL_VERSION"
[ -f "$STAMP" ] && { echo "$OUT (up to date)"; exit 0; }

mkdir -p "$SRC"
fetch() { # url file sha256
    [ -f "$SRC/$2" ] || curl -sSfL "$1" -o "$SRC/$2"
    echo "$3  $SRC/$2" | sha256sum -c --quiet - || { echo "checksum mismatch for $2" >&2; rm -f "$SRC/$2"; exit 1; }
}
fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" "ffmpeg-$FFMPEG_VERSION.tar.xz" "$FFMPEG_SHA256"
fetch "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" "dav1d-$DAV1D_VERSION.tar.xz" "$DAV1D_SHA256"
fetch "https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz" "opus-$OPUS_VERSION.tar.gz" "$OPUS_SHA256"
fetch "https://github.com/libsdl-org/SDL/releases/download/release-$SDL_VERSION/SDL3-$SDL_VERSION.tar.gz" "SDL3-$SDL_VERSION.tar.gz" "$SDL_SHA256"
fetch "https://github.com/FFmpeg/nv-codec-headers/releases/download/n$NVHDR_VERSION/nv-codec-headers-$NVHDR_VERSION.tar.gz" "nv-codec-headers-$NVHDR_VERSION.tar.gz" "$NVHDR_SHA256"
fetch "https://github.com/GPUOpen-LibrariesAndSDKs/AMF/releases/download/v$AMF_VERSION/AMF-headers-v$AMF_VERSION.tar.gz" "AMF-headers-v$AMF_VERSION.tar.gz" "$AMF_SHA256"
fetch "https://github.com/intel/libvpl/archive/refs/tags/v$VPL_VERSION.tar.gz" "libvpl-$VPL_VERSION.tar.gz" "$VPL_SHA256"

rm -rf "$OUT"
mkdir -p "$OUT"
docker run --rm -v "$SRC":/src:ro -v "$OUT":/out -e FFV="$FFMPEG_VERSION" -e DAV="$DAV1D_VERSION" -e OPV="$OPUS_VERSION" -e SDLV="$SDL_VERSION" -e NVV="$NVHDR_VERSION" -e AMFV="$AMF_VERSION" -e VPLV="$VPL_VERSION" -e OWNER="$(id -u):$(id -g)" \
    rust:1.97-bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends \
        gcc-mingw-w64-x86-64-posix g++-mingw-w64-x86-64-posix binutils-mingw-w64-x86-64 nasm meson ninja-build cmake pkg-config >/dev/null
    X=x86_64-w64-mingw32
    # Run a build step quietly; on failure show the end of its output.
    run() { "$@" > /tmp/step.log 2>&1 || { echo "FAILED: $*"; tail -40 /tmp/step.log; [ -f ffbuild/config.log ] && tail -40 ffbuild/config.log; exit 1; }; }
    B=/tmp/build; P=/tmp/prefix; mkdir -p $B $P
    cat > $B/cross.txt <<CROSS
[binaries]
c = '"'"'$X-gcc-posix'"'"'
cpp = '"'"'$X-g++-posix'"'"'
ar = '"'"'$X-ar'"'"'
strip = '"'"'$X-strip'"'"'
windres = '"'"'$X-windres'"'"'
pkg-config = '"'"'pkg-config'"'"'
[host_machine]
system = '"'"'windows'"'"'
cpu_family = '"'"'x86_64'"'"'
cpu = '"'"'x86_64'"'"'
endian = '"'"'little'"'"'
CROSS
    # dav1d (static)
    tar -C $B -xJf /src/dav1d-$DAV.tar.xz
    run meson setup $B/dav1d-$DAV $B/dav1d-build --cross-file $B/cross.txt --prefix=$P --libdir=lib --buildtype=release \
        -Ddefault_library=static -Denable_tools=false -Denable_tests=false -Denable_examples=false
    run ninja -C $B/dav1d-build install
    # NVENC headers (FFmpeg loads the driver at run time)
    tar -C $B -xzf /src/nv-codec-headers-$NVV.tar.gz
    run make -C $B/nv-codec-headers-$NVV install PREFIX=$P
    # AMD AMF headers (the runtime comes with the AMD driver)
    tar -C $B -xzf /src/AMF-headers-v$AMFV.tar.gz
    mkdir -p $P/include && cp -r $B/amf-headers-v$AMFV/AMF $P/include/
    # Intel libvpl dispatcher (static; the runtime comes with the Intel driver)
    tar -C $B -xzf /src/libvpl-$VPLV.tar.gz
    run cmake -S $B/libvpl-$VPLV -B $B/vpl-build -G Ninja -DCMAKE_SYSTEM_NAME=Windows -DCMAKE_C_COMPILER=$X-gcc-posix \
        -DCMAKE_CXX_COMPILER=$X-g++-posix -DCMAKE_RC_COMPILER=$X-windres -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=$P \
        -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTS=OFF -DBUILD_EXAMPLES=OFF -DBUILD_EXPERIMENTAL=OFF -DINSTALL_EXAMPLES=OFF -DINSTALL_DEV=ON
    run ninja -C $B/vpl-build install
    # The dispatcher is C++: FFmpeg links with the C compiler, so add libstdc++ (static).
    sed -i "s|^Libs: \(.*\)\$|Libs: \1 -Wl,-Bstatic -lstdc++ -Wl,-Bdynamic|" $P/lib/pkgconfig/vpl.pc
    # FFmpeg (shared, decoders only)
    tar -C $B -xJf /src/ffmpeg-$FFV.tar.xz
    cd $B/ffmpeg-$FFV
    export PKG_CONFIG_PATH=$P/lib/pkgconfig PKG_CONFIG_LIBDIR=$P/lib/pkgconfig
    run ./configure --prefix=$P \
        --target-os=mingw32 --arch=x86_64 --enable-cross-compile --cross-prefix=$X- --cc=$X-gcc-posix --cxx=$X-g++-posix \
        --enable-shared --disable-static \
        --disable-programs --disable-doc --disable-network --disable-autodetect \
        --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample \
        --disable-everything \
        --enable-decoder=h264,hevc,av1,libdav1d --enable-parser=h264,hevc,av1 \
        --enable-libdav1d --enable-w32threads \
        --enable-ffnvcodec --enable-nvenc --enable-amf --enable-libvpl \
        --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc,h264_amf,hevc_amf,av1_amf,h264_qsv,hevc_qsv,av1_qsv \
        --enable-d3d11va --enable-dxva2 \
        --enable-hwaccel=h264_d3d11va,hevc_d3d11va,av1_d3d11va,h264_d3d11va2,hevc_d3d11va2,av1_d3d11va2,h264_dxva2,hevc_dxva2,av1_dxva2 \
        --pkg-config=pkg-config --pkg-config-flags=--static \
        --extra-ldflags="-static-libgcc -static-libstdc++ -Wl,-Bstatic -lwinpthread -Wl,-Bdynamic"
    run make -j"$(nproc)"
    run make install
    cp $P/bin/avcodec-*.dll $P/bin/avutil-*.dll /out/
    mkdir -p /out/LICENSES
    cp COPYING.LGPLv2.1 /out/LICENSES/FFmpeg-LGPL-2.1.txt
    cp $B/dav1d-$DAV/COPYING /out/LICENSES/dav1d-BSD-2-Clause.txt
    cp $B/libvpl-$VPLV/LICENSE /out/LICENSES/libvpl-MIT.txt
    head -30 $B/amf-headers-v$AMFV/AMF/core/Version.h > /out/LICENSES/AMF-MIT.txt
    head -30 $B/nv-codec-headers-$NVV/include/ffnvcodec/nvEncodeAPI.h > /out/LICENSES/nv-codec-headers-MIT.txt
    # Opus
    tar -C $B -xzf /src/opus-$OPV.tar.gz
    run cmake -S $B/opus-$OPV -B $B/opus-build -G Ninja -DCMAKE_SYSTEM_NAME=Windows -DCMAKE_C_COMPILER=$X-gcc-posix \
        -DCMAKE_RC_COMPILER=$X-windres -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
        -DOPUS_BUILD_PROGRAMS=OFF -DOPUS_BUILD_TESTING=OFF -DOPUS_STACK_PROTECTOR=OFF -DOPUS_FORTIFY_SOURCE=OFF \
        -DCMAKE_SHARED_LINKER_FLAGS="-static-libgcc"
    run ninja -C $B/opus-build
    cp "$(ls $B/opus-build/*opus*.dll | head -1)" /out/opus.dll
    cp $B/opus-$OPV/COPYING /out/LICENSES/Opus-BSD-3-Clause.txt
    # SDL3, game controllers only (HIDAPI, RawInput, XInput, Windows.Gaming.Input)
    tar -C $B -xzf /src/SDL3-$SDLV.tar.gz
    run cmake -S $B/SDL3-$SDLV -B $B/sdl-build -G Ninja -DCMAKE_SYSTEM_NAME=Windows -DCMAKE_C_COMPILER=$X-gcc-posix \
        -DCMAKE_CXX_COMPILER=$X-g++-posix -DCMAKE_RC_COMPILER=$X-windres -DCMAKE_BUILD_TYPE=Release \
        -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TEST_LIBRARY=OFF -DSDL_TESTS=OFF -DSDL_EXAMPLES=OFF \
        -DSDL_AUDIO=OFF -DSDL_VIDEO=OFF -DSDL_RENDER=OFF -DSDL_GPU=OFF -DSDL_CAMERA=OFF \
        -DCMAKE_SHARED_LINKER_FLAGS="-static-libgcc"
    run ninja -C $B/sdl-build
    cp $B/sdl-build/SDL3.dll /out/
    cp $B/SDL3-$SDLV/LICENSE.txt /out/LICENSES/SDL3-zlib.txt
    # MinGW thread runtime used by avutil (MIT licence)
    cp /usr/$X/lib/libwinpthread-1.dll /out/
    for f in /usr/share/doc/mingw-w64-common/copyright /usr/share/doc/libwinpthread-mingw-w64-dev/copyright; do
        [ -f $f ] && { cp $f /out/LICENSES/MinGW-w64-winpthreads.txt; break; }
    done
    $X-strip --strip-unneeded /out/*.dll
    printf "FFmpeg %s (LGPL 2.1, https://ffmpeg.org) with dav1d %s (BSD 2-Clause), libvpl %s (MIT), NVENC headers %s and AMF headers %s (MIT), Opus %s (BSD 3-Clause, https://opus-codec.org) and SDL %s (zlib, https://libsdl.org).\nBuilt by scripts/build-windows-deps.sh from the unmodified release tarballs.\n" $FFV $DAV $VPLV $NVV $AMFV $OPV $SDLV > /out/LICENSES/README.txt
    echo "== DLL imports"
    for d in /out/*.dll; do echo "$(basename $d): $($X-objdump -p $d | sed -n "s/.*DLL Name: //p" | sort -u | tr "\n" " ")"; done
    chown -R $OWNER /out
'
touch "$STAMP"
ls -la "$OUT"
