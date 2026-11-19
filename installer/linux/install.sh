#!/bin/sh
# 2moon client for Linux: install for the current user, no root needed.
#   ./install.sh                 install (or update) from this extracted archive
#   ./install.sh --no-desktop    same, without the shortcut on the desktop
#   ./install.sh --uninstall     remove it (settings and paired hosts are kept)
#
# Installs into ~/.local/share/2moon/client, adds 2moon to the applications menu with
# its icon (the two moons), a shortcut on the desktop, and the moon-client command in
# ~/.local/bin.
set -eu

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
DEST="$DATA/2moon/client"
BINDIR="$HOME/.local/bin"
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor"
DESKTOP_DIR="$(xdg-user-dir DESKTOP 2>/dev/null || true)"
[ -n "$DESKTOP_DIR" ] && [ "$DESKTOP_DIR" != "$HOME" ] || DESKTOP_DIR="$HOME/Desktop"

refresh() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$APPS" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$ICONS" 2>/dev/null || true
    for k in kbuildsycoca6 kbuildsycoca5; do
        command -v "$k" >/dev/null 2>&1 && { "$k" >/dev/null 2>&1 || true; break; }
    done
}

remove_files() {
    rm -f "$APPS/2moon.desktop" "$DESKTOP_DIR/2moon.desktop" \
        "$ICONS/scalable/apps/2moon.svg" "$ICONS/256x256/apps/2moon.png"
    # Only our own symlink, never a moon-client built or installed some other way.
    if [ -L "$BINDIR/moon-client" ] && [ "$(readlink "$BINDIR/moon-client")" = "$DEST/moon-client" ]; then
        rm -f "$BINDIR/moon-client"
    fi
}

DESKTOP=1
case "${1:-}" in
    --uninstall)
        remove_files
        rm -rf "$DEST"
        rmdir "$DATA/2moon" 2>/dev/null || true
        refresh
        echo "2moon client removed (settings and paired hosts are kept in ~/.config/2moon-client)."
        exit 0 ;;
    --no-desktop) DESKTOP=0 ;;
    "") ;;
    *) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac

SRC="$(cd "$(dirname "$0")" && pwd)"
[ -f "$SRC/moon-client" ] || { echo "moon-client not found next to $0: run it from the extracted archive." >&2; exit 1; }
if [ "$SRC" = "$DEST" ]; then
    echo "Already installed in $DEST (run ./install.sh --uninstall to remove it)." >&2
    exit 1
fi

# Fresh copy: an update replaces the previous version (libraries included).
rm -rf "$DEST"
mkdir -p "$DEST" "$BINDIR" "$APPS" "$ICONS/scalable/apps" "$ICONS/256x256/apps"
cp -a "$SRC/moon-client" "$SRC/lib" "$SRC/LICENSES" "$SRC/install.sh" "$DEST/"
for f in README.md LICENSE 2moon.svg 2moon.png; do
    if [ -f "$SRC/$f" ]; then cp -a "$SRC/$f" "$DEST/"; fi
done
cp "$SRC/2moon.svg" "$ICONS/scalable/apps/2moon.svg"
cp "$SRC/2moon.png" "$ICONS/256x256/apps/2moon.png"

# The client's window uses the app id "2moon": the compositor matches it with
# 2moon.desktop, so the taskbar shows the same icon.
cat >"$APPS/2moon.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=2moon
GenericName=Game streaming
GenericName[fr]=Streaming de jeux
Comment=Play the games of your PC from another computer
Comment[fr]=Jouer aux jeux de votre PC depuis un autre ordinateur
Exec="$DEST/moon-client"
TryExec=$DEST/moon-client
Icon=2moon
Terminal=false
Categories=Game;
Keywords=stream;streaming;moonlight;sunshine;remote;game;
StartupWMClass=2moon
StartupNotify=true
EOF

# The moon-client command (pair, list, stream... see README.md); found in PATH on most
# distributions, which add ~/.local/bin when it exists.
if [ ! -e "$BINDIR/moon-client" ] || [ -L "$BINDIR/moon-client" ]; then
    ln -sfn "$DEST/moon-client" "$BINDIR/moon-client"
fi

if [ "$DESKTOP" = 0 ]; then
    rm -f "$DESKTOP_DIR/2moon.desktop"
elif [ -d "$DESKTOP_DIR" ]; then
    cp "$APPS/2moon.desktop" "$DESKTOP_DIR/2moon.desktop"
    # KDE and GNOME only launch desktop shortcuts that are executable; GNOME also
    # wants them marked trusted.
    chmod +x "$DESKTOP_DIR/2moon.desktop"
    command -v gio >/dev/null 2>&1 && gio set "$DESKTOP_DIR/2moon.desktop" metadata::trusted true 2>/dev/null || true
fi

refresh
echo "2moon client installed in $DEST"
echo "Launch it from the applications menu (2moon) or the desktop shortcut."
echo "Uninstall: $DEST/install.sh --uninstall"
