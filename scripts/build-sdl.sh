#!/usr/bin/env bash
# Build SDL3 for Linux, game controllers only, in a Debian 12 container.
#   scripts/build-sdl.sh -> target/sdl-bundle/linux-x86_64/{lib/libSDL3.so.0,LICENSES}
# Source checked against the pinned SHA-256 below.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDL_VERSION=3.4.20
SDL_SHA256=f3bb06f55bed9557227a700af2ea62e49497ebdf58037732a731a5fb3ef8581f

SRC="$ROOT/target/ffmpeg-src"
OUT="$ROOT/target/sdl-bundle/linux-x86_64"
STAMP="$OUT/.built-sdl-$SDL_VERSION"
[ -f "$STAMP" ] && { echo "$OUT (up to date)"; exit 0; }

mkdir -p "$SRC"
f="SDL3-$SDL_VERSION.tar.gz"
[ -f "$SRC/$f" ] || curl -sSfL "https://github.com/libsdl-org/SDL/releases/download/release-$SDL_VERSION/$f" -o "$SRC/$f"
echo "$SDL_SHA256  $SRC/$f" | sha256sum -c --quiet - || { echo "checksum mismatch for $f" >&2; rm -f "$SRC/$f"; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT"
docker run --rm -v "$SRC":/src:ro -v "$OUT":/out -e V="$SDL_VERSION" -e OWNER="$(id -u):$(id -g)" \
    rust:1.97-bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends cmake ninja-build pkg-config libudev-dev >/dev/null
    run() { "$@" > /tmp/step.log 2>&1 || { echo "FAILED: $*"; tail -40 /tmp/step.log; exit 1; }; }
    B=/tmp/build; mkdir -p $B
    tar -C $B -xzf /src/SDL3-$V.tar.gz
    run cmake -S $B/SDL3-$V -B $B/sdl-build -G Ninja -DCMAKE_BUILD_TYPE=Release \
        -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TEST_LIBRARY=OFF -DSDL_TESTS=OFF -DSDL_EXAMPLES=OFF \
        -DSDL_AUDIO=OFF -DSDL_VIDEO=OFF -DSDL_RENDER=OFF -DSDL_GPU=OFF -DSDL_CAMERA=OFF \
        -DSDL_HIDAPI=ON -DSDL_HIDAPI_LIBUSB=OFF -DSDL_UNIX_CONSOLE_BUILD=ON
    run ninja -C $B/sdl-build
    mkdir -p /out/lib /out/LICENSES
    cp -L $B/sdl-build/libSDL3.so.0 /out/lib/libSDL3.so.0
    strip --strip-unneeded /out/lib/libSDL3.so.0
    cp $B/SDL3-$V/LICENSE.txt /out/LICENSES/SDL3-zlib.txt
    printf "SDL %s (zlib licence, https://libsdl.org), built by scripts/build-sdl.sh from the unmodified release tarball; game controllers only.\n" $V > /out/LICENSES/SDL3-README.txt
    chown -R $OWNER /out
'
touch "$STAMP"
ls -la "$OUT/lib"
