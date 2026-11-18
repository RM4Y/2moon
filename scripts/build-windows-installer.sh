#!/usr/bin/env bash
# Build the Windows host installer (NSIS) in a Debian 12 container:
#   scripts/build-windows-installer.sh <version> [output-dir]
# -> <output-dir>/2moon-host-<version>-windows-x86_64-setup.exe
#
# Needs the Windows build (scripts/build-windows.sh) and its DLLs
# (scripts/build-windows-deps.sh); bundles the ViGEmBus driver installer
# (BSD 3-Clause, Nefarius Software Solutions) for gamepads.
set -eu
VERSION="${1:?usage: $0 <version> [output-dir]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist}"
VIGEM_VERSION=1.22.0
VIGEM_SHA256=89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a
VIGEM_FILE="ViGEmBus_${VIGEM_VERSION}_x64_x86_arm64.exe"

SRC="$ROOT/target/ffmpeg-src"
mkdir -p "$SRC" "$OUT"
[ -f "$SRC/$VIGEM_FILE" ] || curl -sSfL "https://github.com/nefarius/ViGEmBus/releases/download/v$VIGEM_VERSION/$VIGEM_FILE" -o "$SRC/$VIGEM_FILE"
echo "$VIGEM_SHA256  $SRC/$VIGEM_FILE" | sha256sum -c --quiet - || { echo "checksum mismatch for $VIGEM_FILE" >&2; rm -f "$SRC/$VIGEM_FILE"; exit 1; }
[ -f "$SRC/ViGEmBus-LICENSE.txt" ] || curl -sSfL "https://raw.githubusercontent.com/nefarius/ViGEmBus/v$VIGEM_VERSION/LICENSE" -o "$SRC/ViGEmBus-LICENSE.txt"

EXE="$ROOT/target/windows/x86_64-pc-windows-gnu/release/moon-server.exe"
DEPS="$ROOT/target/windows-deps"
[ -f "$EXE" ] || { echo "missing $EXE (run scripts/build-windows.sh)" >&2; exit 1; }
[ -f "$DEPS/avcodec-62.dll" ] || { echo "missing $DEPS (run scripts/build-windows-deps.sh)" >&2; exit 1; }

STAGE="$ROOT/target/windows-installer/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/LICENSES"
cp "$EXE" "$STAGE/"
# The host needs FFmpeg (encoders), Opus and the MinGW thread runtime; not SDL.
cp "$DEPS/avcodec-62.dll" "$DEPS/avutil-60.dll" "$DEPS/opus.dll" "$DEPS/libwinpthread-1.dll" "$STAGE/"
cp -r "$DEPS/LICENSES/." "$STAGE/LICENSES/"
rm -f "$STAGE/LICENSES/SDL3-zlib.txt"
cp "$SRC/ViGEmBus-LICENSE.txt" "$STAGE/LICENSES/ViGEmBus-BSD-3-Clause.txt"
cp "$SRC/$VIGEM_FILE" "$STAGE/ViGEmBus_setup.exe"
cp "$ROOT/LICENSE" "$STAGE/LICENSE.txt"
cp "$ROOT/installer/windows/README.txt" "$STAGE/README.txt"

NAME="2moon-host-$VERSION-windows-x86_64-setup.exe"
# NSIS wants a numeric version (a.b.c): drop any -dev.N suffix there.
NUMERIC="$(echo "${VERSION#v}" | sed 's/-.*//')"
docker run --rm -v "$ROOT":/src -w /src/installer/windows -e OWNER="$(id -u):$(id -g)" debian:bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends nsis binutils-mingw-w64-x86-64 >/dev/null
    x86_64-w64-mingw32-strip /src/target/windows-installer/stage/moon-server.exe
    makensis -V2 -DVERSION='"$NUMERIC"' -DSTAGE=/src/target/windows-installer/stage -DOUTFILE=/src/target/windows-installer/'"$NAME"' 2moon-host.nsi
    chown -R $OWNER /src/target/windows-installer
'
cp "$ROOT/target/windows-installer/$NAME" "$OUT/"
(cd "$OUT" && sha256sum "$NAME" > "$NAME.sha256")
echo "$OUT/$NAME"
