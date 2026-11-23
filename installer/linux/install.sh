#!/bin/sh
# 2moon for Linux, installed for the current user (no root).
#   ./install.sh [--no-desktop]   install or update
#   ./install.sh --uninstall      remove (settings kept)
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
    for b in moon-client moon-server; do
        if [ -L "$BINDIR/$b" ] && [ "$(readlink "$BINDIR/$b")" = "$DEST/$b" ]; then
            rm -f "$BINDIR/$b"
        fi
    done
}

# The host service set up by the app, when it runs the copy installed here.
UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/2moon-server.service"
remove_service() {
    if [ -f "$UNIT" ] && grep -q "$DEST/moon-server" "$UNIT"; then
        systemctl --user disable --now 2moon-server.service >/dev/null 2>&1 || true
        rm -f "$UNIT"
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
}

DESKTOP=1
case "${1:-}" in
    --uninstall)
        remove_service
        remove_files
        rm -rf "$DEST"
        rmdir "$DATA/2moon" 2>/dev/null || true
        refresh
        echo "2moon removed (settings, paired hosts and host data are kept in ~/.config/2moon-client and ~/.config/2moon-server)."
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

# Fresh copy: an update replaces the previous version (libraries included). A running
# host is stopped meanwhile and started again afterwards.
HOST_WAS_RUNNING=0
if systemctl --user is-active --quiet 2moon-server.service 2>/dev/null && [ -f "$UNIT" ] && grep -q "$DEST/moon-server" "$UNIT"; then
    HOST_WAS_RUNNING=1
    systemctl --user stop 2moon-server.service || true
fi
rm -rf "$DEST"
mkdir -p "$DEST" "$BINDIR" "$APPS" "$ICONS/scalable/apps" "$ICONS/256x256/apps"
cp -a "$SRC/moon-client" "$SRC/lib" "$SRC/LICENSES" "$SRC/install.sh" "$DEST/"
if [ -f "$SRC/moon-server" ]; then cp -a "$SRC/moon-server" "$DEST/"; fi
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
Keywords=stream;streaming;gamestream;remote;game;
StartupWMClass=2moon
StartupNotify=true
EOF

# The moon-client command (pair, list, stream... see README.md); found in PATH on most
# distributions, which add ~/.local/bin when it exists.
for b in moon-client moon-server; do
    if [ -f "$DEST/$b" ] && { [ ! -e "$BINDIR/$b" ] || [ -L "$BINDIR/$b" ]; }; then
        ln -sfn "$DEST/$b" "$BINDIR/$b"
    fi
done
[ "$HOST_WAS_RUNNING" = 0 ] || systemctl --user start 2moon-server.service || true

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
echo "2moon installed in $DEST"
echo "Launch it from the applications menu (2moon) or the desktop shortcut;"
echo "switch to Host mode (top right) to share this computer."
echo "Uninstall: $DEST/install.sh --uninstall"
