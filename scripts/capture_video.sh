#!/usr/bin/env bash
# Capture a short MJPEG video from the board's `mjpeg_test` firmware.
#
# Usage: scripts/capture_video.sh [max_wait_seconds] [serial_port]
#
# Assumes `mjpeg_test` (not `firmware`/main.rs) is already flashed and
# running -- it captures a fixed number of frames on boot, hex-dumping each
# one to serial right after capture. This script waits for the "MJPEG
# CAPTURE DONE" marker, extracts each frame via the Rust server utility, then uses
# ffmpeg to assemble them into an H.264 .mp4 at the camera's actual
# capture-only FPS (reported by the firmware, excluding serial transmission
# time) so the video plays back at a realistic speed.
#
# NOTE: an earlier version produced an MJPEG-codec .avi -- the frames were
# valid (confirmed via direct pixel inspection) but QuickTime Player has
# broken support for MJPEG-in-AVI and renders it as a black frame. H.264/mp4
# plays natively everywhere on macOS.
set -euo pipefail

MAX_WAIT="${1:-120}"
PORT="${2:-$(ls /dev/cu.usbmodem* 2>/dev/null | head -1)}"

if [ -z "$PORT" ]; then
    echo "no /dev/cu.usbmodem* serial port found -- is the board plugged in?" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG="$(mktemp -t capture_video).log"
# Individual burst frames are throwaway intermediates -- keep them in a temp
# dir (auto-cleaned below) instead of cluttering the Desktop. Only the final
# assembled video is worth keeping.
FRAME_DIR="$(mktemp -d -t capture_video_frames)"
OUT_DIR="$HOME/Desktop"
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
FRAME_PREFIX="$FRAME_DIR/frame.jpg"
VIDEO_OUT="$OUT_DIR/mjpeg_video_$TIMESTAMP.mp4"

cleanup() {
    rm -rf "$FRAME_DIR"
}
trap cleanup EXIT

echo "listening on $PORT for up to ${MAX_WAIT}s -- reset/replug the board if it's not already running mjpeg_test..."

stty -f "$PORT" 115200 raw -echo
cat "$PORT" > "$LOG" &
CAT_PID=$!

ELAPSED=0
while [ "$ELAPSED" -lt "$MAX_WAIT" ]; do
    if grep -q "MJPEG CAPTURE DONE" "$LOG" 2>/dev/null; then
        sleep 1 # let the FPS summary line finish writing
        break
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))
done

kill "$CAT_PID" 2>/dev/null || true
wait "$CAT_PID" 2>/dev/null || true

if ! grep -q "MJPEG CAPTURE DONE" "$LOG" 2>/dev/null; then
    echo "capture did not complete within ${MAX_WAIT}s -- see $LOG"
    exit 1
fi

FPS=$(grep -o "camera FPS: [0-9.]*" "$LOG" | tail -1 | awk '{print $3}')
FPS="${FPS:-5}"
echo "camera-reported FPS: $FPS"

FILES=$(cargo run --quiet --release --manifest-path "$REPO_ROOT/Cargo.toml" -p server -- \
    decode-capture "$LOG" "$FRAME_PREFIX") || true

if [ -z "$FILES" ]; then
    echo "no frames found in log -- see $LOG"
    exit 1
fi

FRAME_COUNT=$(echo "$FILES" | wc -l | tr -d ' ')
echo "extracted $FRAME_COUNT frames"

FIRST_FRAME=$(echo "$FILES" | head -1)
PATTERN="${FIRST_FRAME%_0.jpg}_%d.jpg"

ffmpeg -y -loglevel error -framerate "$FPS" -start_number 0 -i "$PATTERN" \
    -c:v libx264 -pix_fmt yuv420p -movflags +faststart "$VIDEO_OUT"

echo "saved video: $VIDEO_OUT"
open "$VIDEO_OUT"
