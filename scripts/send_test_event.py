#!/usr/bin/env python3
"""Sends a JPEG straight to the local `server` as a real event upload
(bypassing the ESP32 board entirely), then waits for and prints the
resulting analysis.json -- a quick way to exercise the server's
storage/AI pipeline against any photo without needing to trigger the
real PIR sensor.

Usage: scripts/send_test_event.py <path-to.jpg> [--server-ip 127.0.0.1]
"""

import argparse
import re
import socket
import struct
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_PATH = REPO_ROOT / "server" / "src" / "config.rs"
UPLOADS_DIR = REPO_ROOT / "server" / "uploads"
SERVER_PORT = 8080

MAGIC = b"CAM1"
VERSION = 2
PART_KIND_THUMBNAIL = 1
ENCODING_JPEG = 1


def read_upload_token() -> str:
    if not CONFIG_PATH.exists():
        sys.exit(f"error: {CONFIG_PATH} not found -- copy config.rs.example and set a real token first")
    match = re.search(r'UPLOAD_TOKEN:\s*&str\s*=\s*"([^"]+)"', CONFIG_PATH.read_text())
    if not match:
        sys.exit(f"error: couldn't find UPLOAD_TOKEN in {CONFIG_PATH}")
    return match.group(1)


def send_event(jpeg_path: Path, server_ip: str, token: str) -> None:
    jpeg_bytes = jpeg_path.read_bytes()
    event_id = int(time.time() * 1000) & 0xFFFFFFFFFFFFFFFF

    envelope_header = MAGIC + struct.pack("<BB", VERSION, 1) + struct.pack("<Q", event_id)
    part_header = struct.pack("<BBIII", PART_KIND_THUMBNAIL, ENCODING_JPEG, 0, 0, len(jpeg_bytes))
    body = envelope_header + part_header + jpeg_bytes

    request_head = (
        f"POST /upload HTTP/1.1\r\n"
        f"Host: {server_ip}:{SERVER_PORT}\r\n"
        f"X-Upload-Token: {token}\r\n"
        f"Content-Length: {len(body)}\r\n"
        f"Connection: close\r\n\r\n"
    ).encode()

    before = time.time()
    with socket.create_connection((server_ip, SERVER_PORT), timeout=10) as s:
        s.sendall(request_head + body)
        response = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            response += chunk

    print(response.decode(errors="replace").splitlines()[0])

    print("waiting for AI analysis...", end="", flush=True)
    deadline = time.time() + 30
    analysis_path = None
    while time.time() < deadline:
        candidates = [p for p in UPLOADS_DIR.glob("*_analysis.json") if p.stat().st_mtime >= before]
        if candidates:
            analysis_path = max(candidates, key=lambda p: p.stat().st_mtime)
            break
        print(".", end="", flush=True)
        time.sleep(1)
    print()

    if analysis_path is None:
        sys.exit("timed out waiting for analysis.json -- is the server running? (cargo run --release -p server)")

    print(f"\n{analysis_path.name}:\n{analysis_path.read_text()}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("jpeg_path", type=Path)
    parser.add_argument("--server-ip", default="127.0.0.1")
    args = parser.parse_args()

    if not args.jpeg_path.exists():
        sys.exit(f"error: {args.jpeg_path} not found")

    send_event(args.jpeg_path, args.server_ip, read_upload_token())
