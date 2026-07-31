#!/usr/bin/env bash
# Capture a PSRAM-recorded video clip from the board's `psram_record_test`
# firmware.
#
# Usage: scripts/capture_psram_video.sh [max_wait_seconds] [serial_port]
#
# Assumes `psram_record_test` is already flashed and running -- it records a
# fixed-duration burst of frames straight into PSRAM with no serial output at
# all during recording, then exports the whole clip as one raw binary dump
# afterward. This script waits for the "RAW EXPORT END" marker, extracts
# frames via the Rust decoder (real per-frame timestamps -> accurate
# FPS, not a guess), assembles them into an H.264 .mp4 via ffmpeg, and opens
# it. Individual frame files are kept in a temp dir and deleted afterward --
# only the final video is saved to the Desktop.
set -euo pipefail

MAX_WAIT="${1:-60}"
PORT="${2:-$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)}"

if [ -z "$PORT" ]; then
    echo "no /dev/cu.usbmodem* serial port found -- is the board plugged in?" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG="$(mktemp -t capture_psram_video).log"
FRAME_DIR="$(mktemp -d -t capture_psram_video_frames)"
OUT_DIR="$HOME/Desktop"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
FRAME_PREFIX="$FRAME_DIR/frame.jpg"
VIDEO_OUT="$OUT_DIR/psram_video_$TIMESTAMP.mp4"

cleanup() {
    rm -rf "$FRAME_DIR"
}
trap cleanup EXIT

echo "listening on $PORT for up to ${MAX_WAIT}s -- reset/replug the board if it's not already running psram_record_test..."

stty -f "$PORT" 115200 raw -echo
cat "$PORT" > "$LOG" &
CAT_PID=$!

ELAPSED=0
while [ "$ELAPSED" -lt "$MAX_WAIT" ]; do
    if grep -q "RAW EXPORT END" "$LOG" 2>/dev/null; then
        sleep 1 # let any trailing output settle
        break
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))
done

kill "$CAT_PID" 2>/dev/null || true
wait "$CAT_PID" 2>/dev/null || true

if ! grep -q "RAW EXPORT END" "$LOG" 2>/dev/null; then
    echo "recording did not complete within ${MAX_WAIT}s -- see $LOG"
    exit 1
fi

DECODE_OUT=$(cargo run --quiet --release --manifest-path "$REPO_ROOT/Cargo.toml" -p server -- \
    decode-raw "$LOG" "$FRAME_PREFIX") || true

if [ -z "$DECODE_OUT" ]; then
    echo "no frames found in raw export -- see $LOG"
    exit 1
fi

FPS=$(echo "$DECODE_OUT" | head -1 | awk '{print $2}')
FILES=$(echo "$DECODE_OUT" | tail -n +2)
FPS="${FPS:-5}"

if [ -z "$FILES" ]; then
    echo "no frames extracted -- see $LOG"
    exit 1
fi

FRAME_COUNT=$(echo "$FILES" | wc -l | tr -d ' ')
echo "extracted $FRAME_COUNT frames, real measured FPS: $FPS"

ffmpeg -y -loglevel error -framerate "$FPS" -start_number 0 -i "$FRAME_DIR/frame_%d.jpg" \
    -c:v libx264 -pix_fmt yuv420p -movflags +faststart "$VIDEO_OUT"

echo "saved video: $VIDEO_OUT"
open "$VIDEO_OUT"
