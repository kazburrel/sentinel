#!/usr/bin/env python3
"""Generate real frame-level FRIDAY lock boxes for one event video.

This is intentionally a local sidecar instead of Rust inference. The Rust
server calls it when a Telegram video is requested. It uses Ultralytics YOLO's
tracking mode (ByteTrack by default) to detect/track objects per frame, writes
`tracks.json`, and renders a new locked MP4 with boxes burned into the video.

Install dependencies later with:

    python3 -m pip install -r scripts/requirements-tracker.txt

The script exits non-zero if the tracker stack is unavailable; the Rust server
then falls back to its plain/older overlay video path.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


PACKAGE_LIKE = {"package", "parcel", "box", "backpack", "handbag", "suitcase"}
CONCERNING_OBJECTS = {"knife", "scissors", "baseball bat"}
VEHICLES = {"car", "truck", "bus", "motorcycle", "bicycle"}
ANIMALS = {"cat", "dog", "bird", "horse", "sheep", "cow"}
REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FACE_MODEL = REPO_ROOT / "scripts" / "models" / "face_detection_yunet_2023mar.onnx"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Create FRIDAY locked video + tracks JSON")
    parser.add_argument("--input", required=True, type=Path, help="Input playable MP4")
    parser.add_argument("--output", required=True, type=Path, help="Output locked MP4")
    parser.add_argument("--tracks", required=True, type=Path, help="Output tracks JSON")
    parser.add_argument("--model", default="yolo11n.pt", help="Ultralytics YOLO model path/name")
    parser.add_argument("--tracker", default="bytetrack.yaml", help="Ultralytics tracker config")
    parser.add_argument("--conf", default=0.25, type=float, help="Detection confidence threshold")
    parser.add_argument("--face-model", default=str(DEFAULT_FACE_MODEL), help="OpenCV YuNet face detector ONNX path")
    parser.add_argument("--face-conf", default=0.45, type=float, help="Face detector confidence threshold")
    parser.add_argument(
        "--threat-level",
        default="normal",
        choices=("normal", "minimal", "threat"),
        help="Event-level context. Person/face color is frame-aware for object threats.",
    )
    parser.add_argument(
        "--threat-hold-frames",
        default=40,
        type=int,
        help="Frames to keep person/face red after a concerning object last appeared.",
    )
    parser.add_argument(
        "--concerning-behavior",
        action="store_true",
        help=(
            "Event analysis flagged concerning_behavior independently of concerning_object. "
            "Used only to decide the post-threat-hold downgrade target (yellow vs white) -- "
            "--threat-level alone can't express 'both are true', since it's collapsed to one "
            "of normal/minimal/threat before reaching this script."
        ),
    )
    return parser.parse_args()


def kind_for_name(name: str) -> str | None:
    normalized = name.strip().lower()
    if normalized == "person":
        return "person"
    if normalized in CONCERNING_OBJECTS:
        return "knife" if normalized == "knife" else "object"
    if normalized in PACKAGE_LIKE:
        return "package"
    if normalized in VEHICLES:
        return "vehicle"
    if normalized in ANIMALS:
        return "animal"
    return None


class ThreatLatch:
    """Holds person/face at "threat" (red) for `hold_frames` frames after a
    concerning object was last seen in any frame, instead of reverting the
    instant the object drops out of a single frame's detections. Without
    this, one continuous threat episode with sparse per-frame object
    detections rendered as a red flicker that snapped straight back to
    white, even though the same person was still on camera in the same
    episode. Any reappearance of the object while still in the hold window
    resets the counter back to `hold_frames` rather than adding to it.

    Once the hold expires, the downgrade target depends on whether an
    object ever actually appeared this clip:
    - If one did, downgrade to yellow only if `concerning_behavior` is also
      true for the event, else white. `concerning_behavior` is deliberately
      not consulted before an object ever appears -- `event_threat_level`
      alone (e.g. "threat", because concerning_object is true) doesn't
      capture that concerning_behavior is *also* true, and using it from
      frame 1 would reintroduce the same "color bleeds in before real
      per-frame evidence" bug the frame-aware fix removed, just in yellow
      instead of red.
    - If no object ever appeared (a gesture-only "minimal" event, with no
      local gesture detector yet), fall back to the event-level context for
      every frame, matching this script's pre-latch behavior for that case.
    """

    def __init__(self, hold_frames: int, event_threat_level: str, concerning_behavior: bool) -> None:
        self.hold_frames = hold_frames
        self.event_threat_level = event_threat_level
        self.concerning_behavior = concerning_behavior
        self.remaining = 0
        self.ever_had_object = False

    def update(self, detections: list[dict[str, Any]]) -> str:
        has_object = any(det["kind"] in {"knife", "object"} for det in detections)
        if has_object:
            self.remaining = self.hold_frames
            self.ever_had_object = True
            return "threat"
        if self.remaining > 0:
            self.remaining -= 1
            return "threat"
        if self.ever_had_object:
            # Hold just expired after a real threat episode: downgrade to
            # yellow only if the event is *also* flagged for concerning
            # behavior, otherwise back to white.
            return "minimal" if self.concerning_behavior else "normal"
        if self.event_threat_level == "minimal":
            # No object has appeared this clip at all -- we do not have a
            # reliable local gesture detector yet, so gesture-only concern
            # still comes from the event-level AI analysis, held for the
            # whole clip rather than per frame.
            return "minimal"
        return "normal"


def color_for_kind(kind: str, frame_level: str) -> tuple[int, int, int]:
    # OpenCV uses BGR.
    if kind in {"knife", "object"}:
        return (0, 0, 255)
    if kind == "package":
        return (0, 255, 0)
    if kind in {"face", "head", "person"} and frame_level == "threat":
        return (0, 0, 255)
    if kind in {"face", "head", "person"} and frame_level == "minimal":
        return (0, 255, 255)
    return (255, 255, 255)


def draw_label(frame: Any, text: str, x1: int, y1: int, color: tuple[int, int, int]) -> None:
    import cv2

    font = cv2.FONT_HERSHEY_SIMPLEX
    scale = 0.55
    thickness = 2
    (tw, th), baseline = cv2.getTextSize(text, font, scale, thickness)
    label_y = max(0, y1 - th - baseline - 6)
    cv2.rectangle(frame, (x1, label_y), (x1 + tw + 10, label_y + th + baseline + 8), (0, 0, 0), -1)
    cv2.putText(frame, text, (x1 + 5, label_y + th + 2), font, scale, color, thickness, cv2.LINE_AA)


def draw_box(frame: Any, det: dict[str, Any], frame_level: str) -> None:
    import cv2

    color = color_for_kind(det["kind"], frame_level)
    x1, y1, x2, y2 = int(det["x1"]), int(det["y1"]), int(det["x2"]), int(det["y2"])
    cv2.rectangle(frame, (x1, y1), (x2, y2), color, 3)
    label = det["label"].upper()
    if det.get("track_id") is not None:
        label = f"{label} #{det['track_id']}"
    draw_label(frame, label, x1, y1, color)


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


def unrotate_box(x: float, y: float, w: float, h: float, rotation: int, width: int, height: int) -> tuple[int, int, int, int]:
    if rotation == 0:
        x1, y1, x2, y2 = x, y, x + w, y + h
    elif rotation == 90:
        x1, y1, x2, y2 = y, height - (x + w), y + h, height - x
    elif rotation == 180:
        x1, y1, x2, y2 = width - (x + w), height - (y + h), width - x, height - y
    elif rotation == 270:
        x1, y1, x2, y2 = width - (y + h), x, width - y, x + w
    else:
        raise ValueError(f"unsupported rotation: {rotation}")
    return (
        max(0, min(width - 1, int(round(x1)))),
        max(0, min(height - 1, int(round(y1)))),
        max(0, min(width - 1, int(round(x2)))),
        max(0, min(height - 1, int(round(y2)))),
    )


def box_iou(a: dict[str, Any], b: dict[str, Any]) -> float:
    x1 = max(a["x1"], b["x1"])
    y1 = max(a["y1"], b["y1"])
    x2 = min(a["x2"], b["x2"])
    y2 = min(a["y2"], b["y2"])
    inter = max(0, x2 - x1) * max(0, y2 - y1)
    area_a = max(0, a["x2"] - a["x1"]) * max(0, a["y2"] - a["y1"])
    area_b = max(0, b["x2"] - b["x1"]) * max(0, b["y2"] - b["y1"])
    denom = area_a + area_b - inter
    return inter / denom if denom else 0.0


def contains_center(outer: dict[str, Any], inner: dict[str, Any]) -> bool:
    cx = (inner["x1"] + inner["x2"]) / 2
    cy = (inner["y1"] + inner["y2"]) / 2
    return outer["x1"] <= cx <= outer["x2"] and outer["y1"] <= cy <= outer["y2"]


def status_text_for(frame_level: str) -> str:
    return {
        "normal": "FRIDAY LOCK",
        "minimal": "FRIDAY LOCK - MINIMAL THREAT",
        "threat": "FRIDAY LOCK - THREAT",
    }[frame_level]


class FaceDetector:
    def __init__(self, model_path: Path, threshold: float) -> None:
        import cv2

        self.enabled = model_path.is_file() and hasattr(cv2, "FaceDetectorYN_create")
        self.detector = None
        self.threshold = threshold
        if self.enabled:
            self.detector = cv2.FaceDetectorYN_create(str(model_path), "", (320, 320), threshold, 0.3, 5000)
        else:
            print(f"tracker: face detector disabled; model/API unavailable: {model_path}", file=sys.stderr)

    def detect(self, frame: Any, person_detections: list[dict[str, Any]]) -> list[dict[str, Any]]:
        if not self.enabled or self.detector is None:
            return []

        height, width = frame.shape[:2]
        raw_faces: list[dict[str, Any]] = []
        for rotation in (0, 90, 180, 270):
            rotated = rotate_frame(frame, rotation)
            rh, rw = rotated.shape[:2]
            self.detector.setInputSize((rw, rh))
            _, faces = self.detector.detect(rotated)
            if faces is None:
                continue
            for face in faces:
                x, y, w, h = [float(v) for v in face[:4]]
                score = float(face[-1])
                if score < self.threshold:
                    continue
                x1, y1, x2, y2 = unrotate_box(x, y, w, h, rotation, width, height)
                if x2 <= x1 or y2 <= y1:
                    continue
                raw_faces.append(
                    {
                        "track_id": None,
                        "kind": "face",
                        "label": "face/head",
                        "confidence": round(score, 4),
                        "x1": x1,
                        "y1": y1,
                        "x2": x2,
                        "y2": y2,
                        "source": f"opencv_yunet_rot{rotation}",
                    }
                )

        raw_faces.sort(key=lambda det: (det["confidence"], (det["x2"] - det["x1"]) * (det["y2"] - det["y1"])), reverse=True)
        kept: list[dict[str, Any]] = []
        used_person_tracks: set[int] = set()
        for face in raw_faces:
            if any(box_iou(face, existing) > 0.10 for existing in kept):
                continue
            for person in person_detections:
                if contains_center(person, face):
                    face["track_id"] = person.get("track_id")
                    break
            if face["track_id"] is not None and face["track_id"] in used_person_tracks:
                continue
            if face["track_id"] is not None:
                used_person_tracks.add(face["track_id"])
            kept.append(face)
            if len(kept) >= 3:
                break
        return kept


def h264_finalize(tmp_video: Path, output: Path) -> None:
    if not shutil.which("ffmpeg"):
        tmp_video.replace(output)
        return

    tmp_h264 = output.with_name(f".{output.stem}.h264.tmp.mp4")
    command = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(tmp_video),
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
        "-an",
        str(tmp_h264),
    ]
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode == 0 and tmp_h264.is_file():
        tmp_h264.replace(output)
        tmp_video.unlink(missing_ok=True)
    else:
        print(f"tracker: ffmpeg h264 finalize failed, keeping OpenCV MP4: {result.stderr}", file=sys.stderr)
        tmp_video.replace(output)
        tmp_h264.unlink(missing_ok=True)


def main() -> int:
    args = parse_args()
    if not args.input.is_file():
        print(f"tracker: input not found: {args.input}", file=sys.stderr)
        return 2

    try:
        import cv2
        from ultralytics import YOLO
    except Exception as exc:  # noqa: BLE001 - report import problem to Rust caller.
        print(f"tracker: missing Python dependency: {exc}", file=sys.stderr)
        print("tracker: install with `python3 -m pip install -r scripts/requirements-tracker.txt`", file=sys.stderr)
        return 3

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.tracks.parent.mkdir(parents=True, exist_ok=True)

    cap = cv2.VideoCapture(str(args.input))
    fps = cap.get(cv2.CAP_PROP_FPS) or 10.0
    width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH) or 640)
    height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT) or 480)
    cap.release()
    if width <= 0 or height <= 0:
        print(f"tracker: invalid video dimensions for {args.input}", file=sys.stderr)
        return 4

    tmp_video = args.output.with_name(f".{args.output.stem}.opencv.tmp.mp4")
    tmp_tracks = args.tracks.with_name(f".{args.tracks.name}.tmp")
    writer = cv2.VideoWriter(str(tmp_video), cv2.VideoWriter_fourcc(*"mp4v"), fps, (width, height))
    if not writer.isOpened():
        print(f"tracker: failed to open output writer: {tmp_video}", file=sys.stderr)
        return 5

    model = YOLO(args.model)
    face_detector = FaceDetector(Path(args.face_model), args.face_conf)
    names = model.names if isinstance(model.names, dict) else dict(enumerate(model.names))
    threat_latch = ThreatLatch(args.threat_hold_frames, args.threat_level, args.concerning_behavior)
    frames: list[dict[str, Any]] = []
    frame_count = 0
    detection_count = 0

    results = model.track(
        source=str(args.input),
        stream=True,
        persist=True,
        tracker=args.tracker,
        conf=args.conf,
        verbose=False,
    )

    for frame_index, result in enumerate(results):
        frame = result.orig_img.copy()
        detections: list[dict[str, Any]] = []
        person_detections: list[dict[str, Any]] = []
        boxes = result.boxes
        if boxes is not None:
            for i, box in enumerate(boxes):
                cls_id = int(box.cls[0].item()) if box.cls is not None else -1
                name = str(names.get(cls_id, cls_id)).lower()
                kind = kind_for_name(name)
                if kind is None:
                    continue

                x1, y1, x2, y2 = [int(round(v)) for v in box.xyxy[0].tolist()]
                x1, y1 = max(0, x1), max(0, y1)
                x2, y2 = min(width - 1, x2), min(height - 1, y2)
                if x2 <= x1 or y2 <= y1:
                    continue

                track_id = None
                if box.id is not None:
                    track_id = int(box.id[0].item())

                label = "package" if kind == "package" else ("knife / sharp object" if kind == "knife" else name)
                det = {
                    "track_id": track_id,
                    "kind": kind,
                    "label": label,
                    "confidence": round(float(box.conf[0].item()), 4) if box.conf is not None else 0.0,
                    "x1": x1,
                    "y1": y1,
                    "x2": x2,
                    "y2": y2,
                    "source": "yolo_bytetrack",
                }
                detections.append(det)
                if kind == "person":
                    person_detections.append(det)

        detections.extend(face_detector.detect(frame, person_detections))

        rendered_threat_level = threat_latch.update(detections)
        for det in detections:
            draw_box(frame, det, rendered_threat_level)

        status_text = status_text_for(rendered_threat_level)
        cv2.putText(
            frame,
            status_text,
            (18, 32),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.8,
            (255, 255, 0),
            2,
            cv2.LINE_AA,
        )
        writer.write(frame)
        frames.append(
            {
                "frame_index": frame_index,
                "event_threat_level": args.threat_level,
                "rendered_threat_level": rendered_threat_level,
                "detections": detections,
            }
        )
        detection_count += len(detections)
        frame_count += 1

    writer.release()

    tracks = {
        "schema": "friday.tracks.v1",
        "source_video": str(args.input),
        "model": args.model,
        "tracker": args.tracker,
        "event_threat_level": args.threat_level,
        "render_policy": "stateful-threat-latch-v3",
        "threat_hold_frames": args.threat_hold_frames,
        "concerning_behavior": args.concerning_behavior,
        "fps": fps,
        "width": width,
        "height": height,
        "frame_count": frame_count,
        "detection_count": detection_count,
        "frames": frames,
    }
    tmp_tracks.write_text(json.dumps(tracks, indent=2), encoding="utf-8")
    tmp_tracks.replace(args.tracks)
    h264_finalize(tmp_video, args.output)
    print(
        f"tracker: wrote {args.output} and {args.tracks} "
        f"({frame_count} frame(s), {detection_count} detection(s))"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
