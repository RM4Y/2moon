#!/usr/bin/env bash
# Install 2moon-server as a systemd *user* service that starts with the
# graphical (Plasma) session. Extra arguments are passed to moon-server.
#
#   scripts/install-service.sh --port 57989 --nvenc-preset p7
#
# Manage it with:  systemctl --user status|restart|stop 2moon-server
#                  journalctl --user -u 2moon-server -f
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/moon-server"
[ -x "$BIN" ] || cargo build --release --manifest-path "$ROOT/Cargo.toml"

ARGS="--name 2moon --video kwin --audio desktop $*"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
mkdir -p "$UNIT_DIR"
cat >"$UNIT_DIR/2moon-server.service" <<EOF
[Unit]
Description=2moon game streaming host (GameStream/Moonlight compatible)
Documentation=file://$ROOT/README.md
# Screen capture needs the compositor (KWin) to be running.
After=graphical-session.target pipewire.service
PartOf=graphical-session.target

[Service]
ExecStart=$BIN $ARGS
Environment=WAYLAND_DISPLAY=wayland-0
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=3

[Install]
WantedBy=graphical-session.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now 2moon-server.service
echo "Installed $UNIT_DIR/2moon-server.service"
systemctl --user --no-pager status 2moon-server.service | head -5
