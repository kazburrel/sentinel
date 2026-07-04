#!/usr/bin/env bash
# Wave your hand in front of the PIR sensor and get back the photo it took.
#
# Usage: scripts/capture_photo.sh [max_wait_seconds] [serial_port]
#
# Assumes firmware.rs is already flashed and running. Listens to the serial
# port for up to `max_wait_seconds` (default 20), but stops as soon as a
# capture finishes (a "JPEG END" marker shows up) instead of always waiting
# out the full window, then decodes and opens the most recent frame found.
set -euo pipefail

MAX_WAIT="${1:-20}"
PORT="${2:-$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)}"

if [ -z "$PORT" ]; then
    echo "no /dev/cu.usbmodem* serial port found -- is the board plugged in?" >&2
    exit 1
fi

LOG="$(mktemp -t capture_photo).log"
OUT_DIR="$HOME/Desktop"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
OUT_PREFIX="$OUT_DIR/pir_capture_$TIMESTAMP.jpg"

echo "listening on $PORT for up to ${MAX_WAIT}s -- wave your hand in front of the PIR sensor now..."

stty -f "$PORT" 115200 raw -echo
cat "$PORT" > "$LOG" &
CAT_PID=$!

# Poll for a completed frame instead of always sleeping the full window --
# a capture (motion -> 2 skipped warm-up frames -> real frame -> hex dump
# over serial) usually finishes well under MAX_WAIT.
ELAPSED=0
while [ "$ELAPSED" -lt "$MAX_WAIT" ]; do
    if grep -q "JPEG END" "$LOG" 2>/dev/null; then
        sleep 1 # let any trailing output (e.g. a 2nd frame) settle
        break
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))
done

kill "$CAT_PID" 2>/dev/null || true
wait "$CAT_PID" 2>/dev/null || true

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FILES=$(python3 "$SCRIPT_DIR/decode_capture.py" "$LOG" "$OUT_PREFIX") || true

if [ -z "$FILES" ]; then
    echo "no photo captured in that window -- try again, wave your hand sooner/longer"
    exit 1
fi

echo "saved:"
echo "$FILES"

LAST_FILE=$(echo "$FILES" | tail -1)
open "$LAST_FILE"
