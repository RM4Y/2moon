#!/usr/bin/env bash
# Build the Windows installer (NSIS): the app and the host service.
#   scripts/build-windows-installer.sh <version> [output-dir]
# Needs build-windows.sh and build-windows-deps.sh first.
set -eu
VERSION="${1:?usage: $0 <version> [output-dir]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist}"
VIGEM_VERSION=1.22.0
VIGEM_SHA256=89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a
VIGEM_FILE="ViGEmBus_${VIGEM_VERSION}_x64_x86_arm64.exe"
MESA_VERSION=26.3.0
MESA_SHA256=c505fcc77e8f63a09d7ddac086591d2f542c0698d37fc9e3094685a34d0d9991
MESA_FILE="mesa3d-${MESA_VERSION}-release-msvc.7z"

SRC="$ROOT/target/ffmpeg-src"
mkdir -p "$SRC" "$OUT"
[ -f "$SRC/$VIGEM_FILE" ] || curl -sSfL "https://github.com/nefarius/ViGEmBus/releases/download/v$VIGEM_VERSION/$VIGEM_FILE" -o "$SRC/$VIGEM_FILE"
echo "$VIGEM_SHA256  $SRC/$VIGEM_FILE" | sha256sum -c --quiet - || { echo "checksum mismatch for $VIGEM_FILE" >&2; rm -f "$SRC/$VIGEM_FILE"; exit 1; }
[ -f "$SRC/$MESA_FILE" ] || curl -sSfL "https://github.com/pal1000/mesa-dist-win/releases/download/$MESA_VERSION/$MESA_FILE" -o "$SRC/$MESA_FILE"
echo "$MESA_SHA256  $SRC/$MESA_FILE" | sha256sum -c --quiet - || { echo "checksum mismatch for $MESA_FILE" >&2; rm -f "$SRC/$MESA_FILE"; exit 1; }
[ -f "$SRC/mesa-$MESA_VERSION-license.rst" ] || curl -sSfL "https://gitlab.freedesktop.org/mesa/mesa/-/raw/mesa-$MESA_VERSION/docs/license.rst" -o "$SRC/mesa-$MESA_VERSION-license.rst"
[ -f "$SRC/ViGEmBus-LICENSE.txt" ] || curl -sSfL "https://raw.githubusercontent.com/nefarius/ViGEmBus/v$VIGEM_VERSION/LICENSE" -o "$SRC/ViGEmBus-LICENSE.txt"

EXE="$ROOT/target/windows/x86_64-pc-windows-gnu/release/moon-server.exe"
CLIENT_EXE="$ROOT/target/windows/x86_64-pc-windows-gnu/release/moon-client.exe"
DEPS="$ROOT/target/windows-deps"
[ -f "$EXE" ] || { echo "missing $EXE (run scripts/build-windows.sh)" >&2; exit 1; }
[ -f "$CLIENT_EXE" ] || { echo "missing $CLIENT_EXE (run scripts/build-windows.sh)" >&2; exit 1; }
[ -f "$DEPS/avcodec-62.dll" ] || { echo "missing $DEPS (run scripts/build-windows-deps.sh)" >&2; exit 1; }

STAGE="$ROOT/target/windows-installer/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/LICENSES"
cp "$EXE" "$CLIENT_EXE" "$STAGE/"
# The host needs FFmpeg (encoders), Opus and the MinGW thread runtime; the 2moon app
# installed next to it (client and Host mode) every DLL (decoders, SDL3 too).
cp "$DEPS"/*.dll "$STAGE/"
cp -r "$DEPS/LICENSES/." "$STAGE/LICENSES/"
cp "$SRC/ViGEmBus-LICENSE.txt" "$STAGE/LICENSES/ViGEmBus-BSD-3-Clause.txt"
cp "$SRC/$VIGEM_FILE" "$STAGE/ViGEmBus_setup.exe"
cp "$ROOT/LICENSE" "$STAGE/LICENSE.txt"
cp "$ROOT/installer/windows/README.txt" "$STAGE/README.txt"

# Mesa (extracted in the container below): the installer adds it only if needed.
mkdir -p "$STAGE/mesa"
cp "$SRC/mesa-$MESA_VERSION-license.rst" "$STAGE/mesa/Mesa-license.txt"

NAME="2moon-$VERSION-windows-x86_64-setup.exe"
# NSIS wants a numeric version (a.b.c): drop any -dev.N suffix there.
NUMERIC="$(echo "${VERSION#v}" | sed 's/-.*//')"
docker run --rm -v "$ROOT":/src -w /src/installer/windows -e OWNER="$(id -u):$(id -g)" debian:bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends nsis binutils-mingw-w64-x86-64 p7zip-full >/dev/null
    7z e -y -bd -o/src/target/windows-installer/stage/mesa /src/target/ffmpeg-src/'"$MESA_FILE"' x64/opengl32.dll x64/libgallium_wgl.dll x64/dxil.dll >/dev/null
    x86_64-w64-mingw32-strip /src/target/windows-installer/stage/moon-server.exe /src/target/windows-installer/stage/moon-client.exe
    makensis -V2 -DVERSION='"$NUMERIC"' -DSTAGE=/src/target/windows-installer/stage -DOUTFILE=/src/target/windows-installer/'"$NAME"' 2moon-host.nsi
    chown -R $OWNER /src/target/windows-installer
'
for n in "$NAME"; do
    cp "$ROOT/target/windows-installer/$n" "$OUT/"
    (cd "$OUT" && sha256sum "$n" > "$n.sha256")
    echo "$OUT/$n"
done
