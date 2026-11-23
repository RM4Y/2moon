#!/usr/bin/env bash
# Build the release packages (.deb, .rpm, Arch, Linux tar.gz, host-only tar.gz,
# Windows installer, and the macOS dmg on a Mac).
#   scripts/package-release.sh v0.2.0 [output-dir]
# Linux builds run in Debian 12 containers; NATIVE=1 uses the local toolchain.
set -eu
VERSION="${1:?usage: $0 <version> [output-dir]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist}"
if [ "$(uname -s)" = Darwin ]; then
    # x86_64 Linux and Windows builds from a Mac: containers in x86_64.
    export DOCKER_DEFAULT_PLATFORM="${DOCKER_DEFAULT_PLATFORM:-linux/amd64}"
    [ "${NATIVE:-0}" = 0 ] || { echo "NATIVE=1 builds Linux archives on Linux only" >&2; exit 1; }
    ARCH="linux-x86_64"
else
    ARCH="linux-$(uname -m)"
fi
command -v sha256sum >/dev/null 2>&1 || sha256sum() { shasum -a 256 "$@"; }
if [ "${NATIVE:-0}" = 1 ]; then
    cargo build --release --manifest-path "$ROOT/Cargo.toml"
    BIN="$ROOT/target/release"
else
    # Rust image (+ libudev headers, no longer needed since controllers moved to
    # SDL, which loads libudev at run time; kept so the cached image is reused).
    IMAGE=2moon-build:1.97-bookworm
    docker image inspect "$IMAGE" >/dev/null 2>&1 || printf 'FROM rust:1.97-bookworm\nRUN apt-get update -qq && apt-get install -y -qq libudev-dev >/dev/null && rm -rf /var/lib/apt/lists/*\n' | docker build -q -t "$IMAGE" - >/dev/null
    docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT":/src -w /src \
        -e CARGO_HOME=/src/target/cargo-home -e CARGO_TARGET_DIR=/src/target/bookworm \
        "$IMAGE" cargo build --release --locked
    BIN="$ROOT/target/bookworm/release"
fi
mkdir -p "$OUT"
"$ROOT/scripts/build-ffmpeg.sh" >/dev/null
FFMPEG_BUNDLE="$ROOT/target/ffmpeg-bundle/linux-x86_64"
"$ROOT/scripts/build-sdl.sh" >/dev/null
SDL_BUNDLE="$ROOT/target/sdl-bundle/linux-x86_64"

package() { # name "binaries" [extra files...]
    local name="$1" bin="$2"; shift 2
    local dir="$OUT/2moon-${name:+$name-}$VERSION-$ARCH"
    rm -rf "$dir"
    mkdir -p "$dir"
    for b in $bin; do
        cp "$BIN/$b" "$dir/"
    done
    cp "$ROOT/README.md" "$ROOT/LICENSE" "$dir/"
    for f in "$@"; do mkdir -p "$dir/$(dirname "$f")"; cp "$ROOT/$f" "$dir/$f"; done
    # Bundled FFmpeg (scripts/build-ffmpeg.sh), loaded from lib/: decoders for the
    # client, VAAPI encoders (AMD / Intel) for the host. Client: SDL3 for game
    # controllers too (scripts/build-sdl.sh).
    cp -a "$FFMPEG_BUNDLE/lib" "$dir/lib"
    cp -a "$FFMPEG_BUNDLE/LICENSES" "$dir/LICENSES"
    if [ "$name" != server ]; then
        cp -a "$SDL_BUNDLE/lib/." "$dir/lib/"
        cp -a "$SDL_BUNDLE/LICENSES/." "$dir/LICENSES/"
        # ./install.sh: applications menu entry, desktop shortcut and the 2moon icon.
        cp "$ROOT/installer/linux/install.sh" "$ROOT/installer/linux/2moon.svg" "$dir/"
        cp "$ROOT/crates/moon-client/assets/icon.png" "$dir/2moon.png"
    fi
    # Strip with the Linux binutils (works from a Mac too).
    if [ "${NATIVE:-0}" = 1 ]; then
        (cd "$dir" && strip $bin)
    else
        docker run --rm -u "$(id -u):$(id -g)" -v "$dir":/d -w /d "$IMAGE" strip $bin
    fi
    tar -C "$OUT" -czf "$dir.tar.gz" "$(basename "$dir")"
    (cd "$OUT" && sha256sum "$(basename "$dir").tar.gz" >"$(basename "$dir").tar.gz.sha256")
    rm -rf "$dir"
    echo "$dir.tar.gz"
}

package server moon-server scripts/install-service.sh
# Client and host together: the 2moon app and its Host mode.
package "" "moon-client moon-server"

# Installable Linux packages of the same app (/opt/2moon, menu entry, icon):
# .deb (Debian, Ubuntu...), .rpm (Fedora, openSUSE...), .pkg.tar.zst (Arch...).
NUM="${VERSION#v}"
LSTAGE="$ROOT/target/linux-packages/stage"
rm -rf "$ROOT/target/linux-packages"
mkdir -p "$LSTAGE"
tar -xzf "$OUT/2moon-$VERSION-$ARCH.tar.gz" -C "$LSTAGE" --strip-components=1
# nfpm does not expand variables in the file list: fill them in.
sed -e "s|\${VERSION}|$NUM|g" -e "s|\${STAGE}|/src/target/linux-packages/stage|g" \
    -e "s|src: \./|src: /src/installer/linux/|g" -e "s|: \./postinstall.sh|: /src/installer/linux/postinstall.sh|g" \
    "$ROOT/installer/linux/nfpm.yaml" >"$ROOT/target/linux-packages/nfpm.yaml"
for p in deb rpm archlinux; do
    docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT":/src -w /src \
        goreleaser/nfpm:v2.43.0 package --config target/linux-packages/nfpm.yaml --packager "$p" --target /src/target/linux-packages/ >/dev/null
done
for f in "$ROOT"/target/linux-packages/*.deb "$ROOT"/target/linux-packages/*.rpm "$ROOT"/target/linux-packages/*.pkg.tar.zst; do
    cp "$f" "$OUT/"
    (cd "$OUT" && sha256sum "$(basename "$f")" >"$(basename "$f").sha256")
    echo "$OUT/$(basename "$f")"
done

# Windows: one installer, the 2moon app (client and Host mode) and the host service.
if [ "${WINDOWS:-1}" = 1 ]; then
    "$ROOT/scripts/build-windows-deps.sh" >/dev/null
    "$ROOT/scripts/build-windows.sh" >/dev/null
    "$ROOT/scripts/build-windows-installer.sh" "$VERSION" "$OUT" | tail -1
fi

# macOS (on a Mac): 2moon.app in a dmg, signed ad hoc (first launch: right-click › Open).
if [ "$(uname -s)" = Darwin ] && [ "${MACOS:-1}" = 1 ]; then
    MARCH="$(uname -m)"
    # No local paths (home folder, user name) inside the released binaries.
    RUSTFLAGS="--remap-path-prefix=$HOME=/build" cargo build --release --manifest-path "$ROOT/Cargo.toml" -p moon-client -p moon-server
    # The local host keeps its Screen Recording / Accessibility permissions only while
    # signed with the same certificate (see docs/GUIDE.md, « Hôte macOS »).
    if security find-identity -p codesigning 2>/dev/null | grep -q '"2moon local"'; then
        codesign -f -s "2moon local" "$ROOT/target/release/moon-server" 2>/dev/null || true
    fi
    "$ROOT/scripts/build-ffmpeg-macos.sh" >/dev/null
    "$ROOT/scripts/build-sdl-macos.sh" >/dev/null
    name="2moon-$VERSION-macos-$MARCH"
    work="$ROOT/target/macos-package"
    rm -rf "$work" "$OUT/$name.dmg"
    app="$work/dmg/2moon.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$ROOT/target/release/moon-client" "$ROOT/target/release/moon-server" "$app/Contents/MacOS/"
    strip -x "$app/Contents/MacOS/moon-client" "$app/Contents/MacOS/moon-server" 2>/dev/null
    cp -a "$ROOT/target/ffmpeg-bundle/macos-$MARCH/lib" "$app/Contents/MacOS/lib"
    cp -a "$ROOT/target/ffmpeg-bundle/macos-$MARCH/LICENSES" "$app/Contents/Resources/LICENSES"
    # SDL3 for game controllers (scripts/build-sdl-macos.sh).
    cp "$ROOT/target/sdl-bundle/macos-$MARCH/lib/libSDL3.0.dylib" "$app/Contents/MacOS/lib/"
    cp "$ROOT/target/sdl-bundle/macos-$MARCH/LICENSES/"* "$app/Contents/Resources/LICENSES/"
    cp "$ROOT/README.md" "$ROOT/LICENSE" "$app/Contents/Resources/"
    # Icon: the two moons, every size macOS asks for.
    iconset="$work/2moon.iconset"
    mkdir -p "$iconset"
    for sz in 16 32 64 128 256 512; do
        sips -z $sz $sz "$ROOT/crates/moon-client/assets/icon.png" --out "$iconset/icon_${sz}x${sz}.png" >/dev/null
    done
    for sz in 16 32 128 256; do cp "$iconset/icon_$((sz * 2))x$((sz * 2)).png" "$iconset/icon_${sz}x${sz}@2x.png"; done
    rm -f "$iconset/icon_64x64.png"
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/2moon.icns"
    cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>2moon</string>
  <key>CFBundleDisplayName</key><string>2moon</string>
  <key>CFBundleIdentifier</key><string>io.github.rm4y.2moon</string>
  <key>CFBundleExecutable</key><string>moon-client</string>
  <key>CFBundleIconFile</key><string>2moon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION#v}</string>
  <key>CFBundleVersion</key><string>${VERSION#v}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.games</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>2moon sends your microphone to the computer you play on, when you turn it on.</string>
</dict>
</plist>
PLIST
    # Nested code first (libraries, host), then the app.
    for f in "$app"/Contents/MacOS/lib/*.dylib "$app/Contents/MacOS/moon-server"; do
        [ -L "$f" ] || codesign -f -s - "$f"
    done
    codesign -f -s - "$app"
    codesign --verify --deep --strict "$app"
    ln -s /Applications "$work/dmg/Applications"
    hdiutil create -quiet -volname 2moon -srcfolder "$work/dmg" -fs HFS+ -format UDZO "$OUT/$name.dmg"
    (cd "$OUT" && sha256sum "$name.dmg" >"$name.dmg.sha256")
    echo "$OUT/$name.dmg"
fi
