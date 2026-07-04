#!/usr/bin/env python3
"""Extract JPEG frames from a serial log produced by firmware::hexdump.

The firmware wraps each captured frame's hex dump in `JPEG BEGIN <len>` /
`JPEG END` marker lines. This scans a log file for all such blocks and
writes each one out as a numbered .jpg file next to the given output prefix.
"""
import sys


def extract_frames(log_path):
    frames = []
    in_frame = False
    chunks = []
    with open(log_path, "r", errors="ignore") as f:
        for line in f:
            line = line.strip()
            if line.startswith("JPEG BEGIN"):
                in_frame = True
                chunks = []
                continue
            if line == "JPEG END":
                if in_frame:
                    frames.append(bytes.fromhex("".join(chunks)))
                in_frame = False
                continue
            if in_frame:
                chunks.append(line)
    return frames


def main():
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <log_file> <output_prefix>", file=sys.stderr)
        sys.exit(1)

    log_path, out_prefix = sys.argv[1], sys.argv[2]
    frames = extract_frames(log_path)

    if not frames:
        print("no JPEG frames found in log", file=sys.stderr)
        sys.exit(1)

    written = []
    for i, data in enumerate(frames):
        path = out_prefix if len(frames) == 1 else f"{out_prefix.rsplit('.', 1)[0]}_{i}.jpg"
        with open(path, "wb") as f:
            f.write(data)
        written.append(path)
        print(path)


if __name__ == "__main__":
    main()
