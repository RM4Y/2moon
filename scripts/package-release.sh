#!/usr/bin/env bash
# Build release archives: one for the host (server) and one for the client.
#   scripts/package-release.sh v0.2.0 [output-dir]
#
# Binaries are built inside a Debian 12 container (glibc 2.36) so they run on
# most distributions (Arch, Debian 12+, Ubuntu 23.04+, Fedora 37+...).
# Set NATIVE=1 to build with the local toolchain instead.
set -eu
VERSION="${1:?usage: $0 <version> [output-dir]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist}"
ARCH="linux-$(uname -m)"
if [ "${NATIVE:-0}" = 1 ]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml"
    BIN="$ROOT/target/release"
else
    docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT":/src -w /src \
        -e CARGO_HOME=/src/target/cargo-home -e CARGO_TARGET_DIR=/src/target/bookworm \
        rust:1.97-bookworm cargo build --release --locked
    BIN="$ROOT/target/bookworm/release"
fi
mkdir -p "$OUT"
"$ROOT/scripts/build-ffmpeg.sh" >/dev/null
FFMPEG_BUNDLE="$ROOT/target/ffmpeg-bundle/linux-x86_64"

package() { # name binary [extra files...]
    local name="$1" bin="$2"; shift 2
    local dir="$OUT/2moon-$name-$VERSION-$ARCH"
    rm -rf "$dir"
    mkdir -p "$dir"
    cp "$BIN/$bin" "$dir/"
    strip "$dir/$bin"
    cp "$ROOT/README.md" "$ROOT/LICENSE" "$dir/"
    for f in "$@"; do mkdir -p "$dir/$(dirname "$f")"; cp "$ROOT/$f" "$dir/$f"; done
    # Client: bundled FFmpeg decoders (scripts/build-ffmpeg.sh), loaded from lib/.
    if [ "$name" = client ]; then
        cp -a "$FFMPEG_BUNDLE/lib" "$dir/lib"
        cp -a "$FFMPEG_BUNDLE/LICENSES" "$dir/LICENSES"
    fi
    tar -C "$OUT" -czf "$dir.tar.gz" "$(basename "$dir")"
    (cd "$OUT" && sha256sum "$(basename "$dir").tar.gz" >"$(basename "$dir").tar.gz.sha256")
    rm -rf "$dir"
    echo "$dir.tar.gz"
}

package server moon-server scripts/install-service.sh
package client moon-client
