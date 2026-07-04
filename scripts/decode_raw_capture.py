#!/usr/bin/env python3
"""Extract length-prefixed JPEG frames from a psram_record_test raw export.

Unlike decode_capture.py (which parses a hex-dumped `JPEG BEGIN`/`JPEG END`
text format), this reads the raw binary export produced by
`Printer::write_bytes()` in psram_record_test.rs, framed by a
`RAW EXPORT BEGIN <byte_count>` text line followed by exactly that many raw
bytes, each frame stored as `[frame_len: u32 LE][timestamp_ms: u32 LE]
[frame_len bytes of JPEG]`.

Writes each frame to `<out_prefix>_<i>.jpg` and prints the average FPS
(derived from real per-frame timestamps) followed by each written path, so a
calling script can capture both without a second parse pass.
"""
import struct
import sys


def find_marker(data, marker):
    idx = data.find(marker)
    if idx < 0:
        return None
    line_end = data.find(b"\n", idx)
    if line_end < 0:
        return None
    return idx, line_end


def extract_frames(log_path):
    with open(log_path, "rb") as f:
        data = f.read()

    found = find_marker(data, b"RAW EXPORT BEGIN ")
    if found is None:
        return []
    marker_start, line_end = found

    header = data[marker_start:line_end].decode("ascii", errors="ignore")
    byte_count = int(header.split()[-1])

    payload_start = line_end + 1
    payload = data[payload_start:payload_start + byte_count]

    if len(payload) < byte_count:
        print(
            f"warning: expected {byte_count} raw bytes, only got {len(payload)} "
            "-- log may be truncated",
            file=sys.stderr,
        )

    frames = []
    offset = 0
    while offset + 8 <= len(payload):
        frame_len, timestamp_ms = struct.unpack_from("<II", payload, offset)
        offset += 8
        if offset + frame_len > len(payload):
            print(
                f"warning: frame at offset {offset} claims {frame_len} bytes "
                f"but only {len(payload) - offset} remain -- stopping",
                file=sys.stderr,
            )
            break
        frames.append((timestamp_ms, payload[offset:offset + frame_len]))
        offset += frame_len

    return frames


def main():
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <log_file> <output_prefix>", file=sys.stderr)
        sys.exit(1)

    log_path, out_prefix = sys.argv[1], sys.argv[2]
    frames = extract_frames(log_path)

    if not frames:
        print("no frames found in raw export", file=sys.stderr)
        sys.exit(1)

    if len(frames) > 1:
        total_ms = frames[-1][0] - frames[0][0]
        fps = (len(frames) - 1) / (total_ms / 1000.0) if total_ms > 0 else 0.0
    else:
        fps = 0.0

    print(f"FPS {fps:.2f}")

    stem = out_prefix.rsplit(".", 1)[0]
    for i, (_timestamp_ms, jpeg) in enumerate(frames):
        path = f"{stem}_{i}.jpg"
        with open(path, "wb") as f:
            f.write(jpeg)
        print(path)


if __name__ == "__main__":
    main()
