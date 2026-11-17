#!/usr/bin/env bash
# End-to-end test: Rust host <-> Rust client on localhost.
# Exercises discovery, pairing (good and bad PIN), app list, launch, RTSP,
# encrypted control stream, video (FEC), encrypted audio, input and quit.
# The host runs with a synthetic video/audio source and does not inject input.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
PORT="${PORT:-60989}"
SECS="${SECS:-6}"
WORK="$(mktemp -d)"
trap 'kill $SERVER_PID 2>/dev/null; rm -rf "$WORK"' EXIT

pass=0; fail=0
ok()   { echo "  PASS  $1"; pass=$((pass+1)); }
ko()   { echo "  FAIL  $1"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else ko "$1"; fi; }

echo "== build"
cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" || { echo "build failed"; exit 1; }

echo "== start host on port $PORT"
RUST_LOG=info "$BIN/moon-server" --port "$PORT" --state-dir "$WORK/host" --pin 2468 --name e2e-host \
    --video test --audio sine --no-input >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 50); do grep -q "HTTP on" "$WORK/server.log" && break; sleep 0.1; done
check "host started" 'kill -0 $SERVER_PID'

echo "== discovery"
"$BIN/moon-client" discover --seconds 3 >"$WORK/discover.txt" 2>/dev/null
check "mDNS advertises e2e-host" 'grep -q "^e2e-host" "$WORK/discover.txt"'

C="$BIN/moon-client --state-dir $WORK/client"
echo "== pairing"
$C pair "127.0.0.1:$PORT" --pin 9999 >"$WORK/badpair.txt" 2>&1
check "wrong PIN is rejected" '! grep -q "Paired with" "$WORK/badpair.txt"'
$C pair "127.0.0.1:$PORT" --pin 2468 >"$WORK/pair.txt" 2>&1
check "correct PIN pairs" 'grep -q "Paired with e2e-host (paired status reported by host: true)" "$WORK/pair.txt"'
$C list "127.0.0.1:$PORT" >"$WORK/list.txt" 2>&1
check "app list over HTTPS" 'grep -qP "^1\tDesktop" "$WORK/list.txt"'

echo "== streaming ${SECS}s (720p60, encrypted control+audio, scripted input)"
$C stream "127.0.0.1:$PORT" --headless --seconds "$SECS" --width 1280 --height 720 --bitrate 8000 --test-input >"$WORK/stream.txt" 2>&1
cat "$WORK/stream.txt" | grep -E "^frames|^decode|^sent" | sed 's/^/        /'
dec=$(grep -oP '\d+(?= decoded)' "$WORK/stream.txt" || echo 0)
lost=$(grep -oP '\d+(?= lost, )' "$WORK/stream.txt" | head -1 || echo 99)
apk=$(grep -oP 'audio: \K\d+' "$WORK/stream.txt" || echo 0)
alost=$(grep -oP 'recovered, \K\d+(?= lost)' "$WORK/stream.txt" || echo 99)
min_frames=$(( SECS * 60 * 70 / 100 ))
check "decoded >= $min_frames frames (got $dec)" '[ "${dec:-0}" -ge $min_frames ]'
check "no video frame lost (got $lost)" '[ "${lost:-99}" -eq 0 ]'
check "audio packets received (got $apk)" '[ "${apk:-0}" -ge $(( SECS * 150 )) ]'
check "no audio packet lost/undecryptable (got $alost)" '[ "${alost:-99}" -eq 0 ]'
check "first picture is 1280x720" 'grep -q "first picture Some((1280, 720" "$WORK/stream.txt"'
sleep 0.5
n_in=$(grep -c "input: " "$WORK/server.log")
check "host decoded 10 input events (got $n_in)" '[ "$n_in" -eq 10 ]'
check "keyboard event decoded" 'grep -q "input: Key { down: true, vk: 65, modifiers: 1 }" "$WORK/server.log"'
check "gamepad event decoded" 'grep -q "input: Gamepad(Gamepad { index: 0, active_mask: 1, buttons: 4096" "$WORK/server.log"'
check "control stream used encryption v2" 'grep -q "encryption v2" "$WORK/server.log"'
check "audio was encrypted" 'grep -q "packets, encrypted" "$WORK/server.log"'

echo "== resume + quit"
$C stream "127.0.0.1:$PORT" --headless --seconds 2 --width 640 --height 360 >"$WORK/stream2.txt" 2>&1
check "second session (resume) streams" 'grep -qP "\d+ decoded" "$WORK/stream2.txt" && [ "$(grep -oP "\d+(?= decoded)" "$WORK/stream2.txt")" -gt 30 ]'
check "resumed with new resolution" 'grep -q "first picture Some((640, 360" "$WORK/stream2.txt"'
$C quit "127.0.0.1:$PORT" >/dev/null 2>&1
$C info "127.0.0.1:$PORT" >"$WORK/info.txt" 2>&1
check "host idle after quit" 'grep -q "busy: false" "$WORK/info.txt"'

echo
echo "== $pass passed, $fail failed"
[ $fail -eq 0 ] || { echo "--- server log tail"; tail -20 "$WORK/server.log"; exit 1; }
