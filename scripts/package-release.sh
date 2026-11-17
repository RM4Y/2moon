#!/usr/bin/env bash
# Build release archives: one for the host (server) and one for the client.
#   scripts/package-release.sh v0.1.0 [output-dir]
set -eu
VERSION="${1:?usage: $0 <version> [output-dir]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist}"
ARCH="linux-$(uname -m)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$OUT"

package() { # name binary [extra files...]
    local name="$1" bin="$2"; shift 2
    local dir="$OUT/2moon-$name-$VERSION-$ARCH"
    rm -rf "$dir"
    mkdir -p "$dir"
    cp "$ROOT/target/release/$bin" "$dir/"
    strip "$dir/$bin"
    cp "$ROOT/README.md" "$ROOT/LICENSE" "$dir/"
    for f in "$@"; do mkdir -p "$dir/$(dirname "$f")"; cp "$ROOT/$f" "$dir/$f"; done
    tar -C "$OUT" -czf "$dir.tar.gz" "$(basename "$dir")"
    (cd "$OUT" && sha256sum "$(basename "$dir").tar.gz" >"$(basename "$dir").tar.gz.sha256")
    rm -rf "$dir"
    echo "$dir.tar.gz"
}

package server moon-server scripts/install-service.sh
package client moon-client
