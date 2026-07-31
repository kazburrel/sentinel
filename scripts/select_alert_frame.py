#!/usr/bin/env python3
"""Pick the clearest human alert photo from event keyframes.

The server already extracts a handful of JPEG keyframes for AI analysis.
This sidecar scores those images with OpenCV YuNet face detection, including
rotated views for the sideways camera angle, and copies the best frame to
`event_<id>_alert.jpg`. If no face is found it exits non-zero, letting Rust
fall back to the existing keyframe/thumbnail choice.
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FACE_MODEL = REPO_ROOT / "scripts" / "models" / "face_detection_yunet_2023mar.onnx"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Select best FRIDAY alert frame")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--face-model", default=str(DEFAULT_FACE_MODEL))
    parser.add_argument("--face-conf", default=0.45, type=float)
    parser.add_argument("images", nargs="+", type=Path)
    return parser.parse_args()


def rotate_frame(frame: Any, rotation: int) -> Any:
    import cv2

    if rotation == 0:
        return frame
    if rotation == 90:
        return cv2.rotate(frame, cv2.ROTATE_90_CLOCKWISE)
    if rotation == 180:
        return cv2.rotate(frame, cv2.ROTATE_180)
    if rotation == 270:
        return cv2.rotate(frame, cv2.ROTATE_90_COUNTERCLOCKWISE)
    raise ValueError(f"unsupported rotation: {rotation}")


def best_face_score(image_path: Path, detector: Any, threshold: float) -> float:
    import cv2

    frame = cv2.imread(str(image_path))
    if frame is None:
        return 0.0

    best = 0.0
    for rotation in (0, 90, 180, 270):
        rotated = rotate_frame(frame, rotation)
        height, width = rotated.shape[:2]
        detector.setInputSize((width, height))
        _, faces = detector.detect(rotated)
        if faces is None:
            continue
        for face in faces:
            x, y, w, h = [float(v) for v in face[:4]]
            confidence = float(face[-1])
            if confidence < threshold:
                continue
            # Favor confident, larger, less edge-clipped faces.
            area_ratio = max(0.0, min(1.0, (w * h) / max(1.0, width * height)))
            edge_penalty = 0.75 if x <= 2 or y <= 2 or x + w >= width - 2 or y + h >= height - 2 else 1.0
            best = max(best, confidence * (1.0 + area_ratio * 8.0) * edge_penalty)
    return best


def main() -> int:
    args = parse_args()
    try:
        import cv2
    except Exception as exc:  # noqa: BLE001
        print(f"alert-frame: OpenCV missing: {exc}", file=sys.stderr)
        return 2

    model_path = Path(args.face_model)
    if not model_path.is_file() or not hasattr(cv2, "FaceDetectorYN_create"):
        print(f"alert-frame: face detector unavailable: {model_path}", file=sys.stderr)
        return 3

    detector = cv2.FaceDetectorYN_create(str(model_path), "", (320, 320), args.face_conf, 0.3, 5000)
    candidates: list[tuple[float, Path]] = []
    for image in args.images:
        if image.is_file():
            candidates.append((best_face_score(image, detector, args.face_conf), image))

    candidates.sort(key=lambda item: item[0], reverse=True)
    if not candidates or candidates[0][0] <= 0:
        print("alert-frame: no face detected in candidate frames", file=sys.stderr)
        return 4

    args.output.parent.mkdir(parents=True, exist_ok=True)
    tmp_output = args.output.with_name(f".{args.output.name}.tmp")
    shutil.copyfile(candidates[0][1], tmp_output)
    tmp_output.replace(args.output)
    print(f"alert-frame: selected {candidates[0][1]} score={candidates[0][0]:.3f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
