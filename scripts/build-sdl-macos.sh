#!/usr/bin/env bash
# Build SDL3 for macOS, game controllers only (same release as build-sdl.sh).
#   scripts/build-sdl-macos.sh -> target/sdl-bundle/macos-<arch>/{lib/libSDL3.0.dylib,LICENSES}
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDL_VERSION=3.4.20
SDL_SHA256=f3bb06f55bed9557227a700af2ea62e49497ebdf58037732a731a5fb3ef8581f
ARCH="$(uname -m)"

SRC="$ROOT/target/ffmpeg-src"
OUT="$ROOT/target/sdl-bundle/macos-$ARCH"
STAMP="$OUT/.built-sdl-$SDL_VERSION"
[ -f "$STAMP" ] && { echo "$OUT (up to date)"; exit 0; }

mkdir -p "$SRC"
f="SDL3-$SDL_VERSION.tar.gz"
[ -f "$SRC/$f" ] || curl -sSfL "https://github.com/libsdl-org/SDL/releases/download/release-$SDL_VERSION/$f" -o "$SRC/$f"
echo "$SDL_SHA256  $SRC/$f" | shasum -a 256 -c --quiet - || { echo "checksum mismatch for $f" >&2; rm -f "$SRC/$f"; exit 1; }

VENV="$ROOT/target/macos-buildtools"
[ -x "$VENV/bin/ninja" ] || { python3 -m venv "$VENV" && "$VENV/bin/pip" install -q meson ninja pkgconf; }
[ -x "$VENV/bin/cmake" ] || "$VENV/bin/pip" install -q cmake
export PATH="$VENV/bin:$PATH"

B="$ROOT/target/sdl-build-macos-$ARCH"
rm -rf "$B" "$OUT"
mkdir -p "$B" "$OUT/lib" "$OUT/LICENSES"
tar -C "$B" -xzf "$SRC/$f"
run() { "$@" >"$B/step.log" 2>&1 || { echo "FAILED: $*" >&2; tail -40 "$B/step.log" >&2; exit 1; }; }
run cmake -S "$B/SDL3-$SDL_VERSION" -B "$B/build" -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 -DCMAKE_OSX_ARCHITECTURES="$ARCH" \
    -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TEST_LIBRARY=OFF -DSDL_TESTS=OFF -DSDL_EXAMPLES=OFF \
    -DSDL_AUDIO=OFF -DSDL_VIDEO=OFF -DSDL_RENDER=OFF -DSDL_GPU=OFF -DSDL_CAMERA=OFF \
    -DSDL_HIDAPI=ON -DSDL_HIDAPI_LIBUSB=OFF
run ninja -C "$B/build"
cp -L "$B/build/libSDL3.0.dylib" "$OUT/lib/libSDL3.0.dylib"
strip -x "$OUT/lib/libSDL3.0.dylib"
codesign -f -s - "$OUT/lib/libSDL3.0.dylib"
cp "$B/SDL3-$SDL_VERSION/LICENSE.txt" "$OUT/LICENSES/SDL3-zlib.txt"
printf "SDL %s (zlib licence, https://libsdl.org), built by scripts/build-sdl-macos.sh from the unmodified release tarball; game controllers only.\n" "$SDL_VERSION" >"$OUT/LICENSES/SDL3-README.txt"
touch "$STAMP"
ls -la "$OUT/lib"
