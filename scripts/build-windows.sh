#!/usr/bin/env bash
# Cross-compile the Windows client (x86_64-pc-windows-gnu, MinGW-w64) in a
# Debian 12 container. Output: target/windows/x86_64-pc-windows-gnu/release/moon-client.exe
#   scripts/build-windows.sh [extra cargo args]
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker run --rm -v "$ROOT":/src -w /src -e OWNER="$(id -u):$(id -g)" \
    -e CARGO_HOME=/src/target/cargo-home -e CARGO_TARGET_DIR=/src/target/windows \
    rust:1.97-bookworm bash -euc '
    apt-get update -qq && apt-get install -y -qq --no-install-recommends gcc-mingw-w64-x86-64-posix g++-mingw-w64-x86-64-posix >/dev/null
    rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1
    export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++-posix
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-posix
    # Static C/C++ runtimes: no libgcc / libstdc++ / winpthread DLLs to ship.
    # OpenH264 is C++: link libstdc++ statically too (cc would ask for the DLL).
    export CXXSTDLIB_x86_64_pc_windows_gnu="static=stdc++"
    GCCLIB=$(dirname "$(x86_64-w64-mingw32-g++-posix -print-file-name=libstdc++.a)")
    export RUSTFLAGS="-C target-feature=+crt-static -C link-args=-static -L $GCCLIB"
    status=0
    cargo build --release --locked --target x86_64-pc-windows-gnu -p moon-client "$@" || status=$?
    chown -R $OWNER /src/target/windows /src/target/cargo-home
    exit $status
' "$@"
