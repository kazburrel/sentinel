#!/usr/bin/env python3
"""Generate real frame-level FRIDAY lock boxes for one event video.

This is intentionally a local sidecar instead of Rust inference. The Rust
server calls it when a Telegram video is requested. It uses Ultralytics YOLO's
tracking mode (ByteTrack by default), OpenCV YuNet faces, and MediaPipe hand
landmarks to detect/track objects and gestures per frame, writes `tracks.json`,
and renders a new locked MP4 with boxes burned into the video.

Install dependencies later with:

    python3 -m pip install -r scripts/requirements-tracker.txt

The script exits non-zero if the tracker stack is unavailable; the Rust server
then falls back to its plain/older overlay video path.
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import sys
from collections import deque
from dataclasses import dataclass
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
    parser.add_argument(
        "--imgsz",
        default=960,
        type=int,
        help="YOLO inference size. 960 improves small concerning-object recall over the 640 default.",
    )
    parser.add_argument("--face-model", default=str(DEFAULT_FACE_MODEL), help="OpenCV YuNet face detector ONNX path")
    parser.add_argument("--face-conf", default=0.45, type=float, help="Face detector confidence threshold")
    parser.add_argument(
        "--body-smoothing",
        default=0.30,
        type=float,
        help="EMA weight for new body positions. Lower is steadier; higher follows motion faster.",
    )
    parser.add_argument(
        "--face-smoothing",
        default=0.22,
        type=float,
        help="EMA weight for face position relative to its tracked body.",
    )
    parser.add_argument(
        "--track-hold-frames",
        default=5,
        type=int,
        help="Frames to predict/hold a body or face through a brief missed detection.",
    )
    parser.add_argument("--hand-conf", default=0.35, type=float, help="MediaPipe hand detection confidence")
    parser.add_argument(
        "--gesture-confirm-frames",
        default=2,
        type=int,
        help="Consecutive middle-finger landmark matches required before yellow lock.",
    )
    parser.add_argument(
        "--gesture-hold-frames",
        default=18,
        type=int,
        help="Frames to keep a confirmed gesture's person yellow after the hand disappears.",
    )
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
        "--threat-preroll-frames",
        default=18,
        type=int,
        help="Buffered frames to recolor red immediately before the first delayed object detection.",
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


def clamp(value: float, minimum: float, maximum: float) -> float:
    return max(minimum, min(maximum, value))


def box_center_size(det: dict[str, Any]) -> tuple[float, float, float, float]:
    width = max(1.0, float(det["x2"]) - float(det["x1"]))
    height = max(1.0, float(det["y2"]) - float(det["y1"]))
    return (
        float(det["x1"]) + width / 2.0,
        float(det["y1"]) + height / 2.0,
        width,
        height,
    )


def raw_box(det: dict[str, Any]) -> dict[str, int]:
    return {name: int(det[name]) for name in ("x1", "y1", "x2", "y2")}


def set_box(
    det: dict[str, Any],
    cx: float,
    cy: float,
    box_width: float,
    box_height: float,
    frame_width: int,
    frame_height: int,
) -> None:
    half_width = max(1.0, box_width) / 2.0
    half_height = max(1.0, box_height) / 2.0
    x1 = int(round(clamp(cx - half_width, 0, frame_width - 2)))
    y1 = int(round(clamp(cy - half_height, 0, frame_height - 2)))
    x2 = int(round(clamp(cx + half_width, x1 + 1, frame_width - 1)))
    y2 = int(round(clamp(cy + half_height, y1 + 1, frame_height - 1)))
    det.update({"x1": x1, "y1": y1, "x2": x2, "y2": y2})


@dataclass
class BodyTrackState:
    template: dict[str, Any]
    cx: float
    cy: float
    width: float
    height: float
    velocity_x: float = 0.0
    velocity_y: float = 0.0
    missed: int = 0


class BodyBoxStabilizer:
    """Smooth tracked YOLO person boxes and bridge short detector dropouts.

    ByteTrack supplies identity, but its box coordinates still come directly from
    YOLO on every frame. Rendering those raw coordinates is what made the visible
    lock shake even while the track ID remained stable. This layer keeps a small
    amount of temporal state per ID, smooths center/size independently, limits a
    single-frame center jump, and predicts for a few missing frames.
    """

    def __init__(self, smoothing: float, hold_frames: int) -> None:
        self.smoothing = clamp(smoothing, 0.05, 1.0)
        self.size_smoothing = clamp(smoothing * 0.65, 0.05, 1.0)
        self.hold_frames = max(0, hold_frames)
        self.states: dict[int, BodyTrackState] = {}

    def update(
        self,
        detections: list[dict[str, Any]],
        frame_width: int,
        frame_height: int,
    ) -> list[dict[str, Any]]:
        output: list[dict[str, Any]] = []
        seen: set[int] = set()

        for detection in detections:
            track_id = detection.get("track_id")
            if track_id is None:
                passthrough = detection.copy()
                passthrough["raw_box"] = raw_box(detection)
                passthrough["stabilized"] = False
                passthrough["held"] = False
                output.append(passthrough)
                continue

            source_track_id = int(track_id)
            track_id = source_track_id
            if track_id not in self.states:
                best_match: tuple[float, int] | None = None
                for existing_id, existing_state in self.states.items():
                    if existing_id in seen:
                        continue
                    existing_box = existing_state.template.copy()
                    set_box(
                        existing_box,
                        existing_state.cx,
                        existing_state.cy,
                        existing_state.width,
                        existing_state.height,
                        frame_width,
                        frame_height,
                    )
                    score = intersection_over_smaller(detection, existing_box)
                    if score > 0.55 and (best_match is None or score > best_match[0]):
                        best_match = (score, existing_id)
                if best_match is not None:
                    track_id = best_match[1]

            detection = detection.copy()
            detection["source_track_id"] = source_track_id
            detection["track_id"] = track_id
            seen.add(track_id)
            measured_cx, measured_cy, measured_width, measured_height = box_center_size(detection)
            state = self.states.get(track_id)
            if state is None:
                state = BodyTrackState(
                    template=detection.copy(),
                    cx=measured_cx,
                    cy=measured_cy,
                    width=measured_width,
                    height=measured_height,
                )
                self.states[track_id] = state
            else:
                delta_x = measured_cx - state.cx
                delta_y = measured_cy - state.cy
                distance = math.hypot(delta_x, delta_y)
                max_step = max(18.0, max(state.width, state.height) * 0.45)
                if distance > max_step:
                    scale = max_step / distance
                    delta_x *= scale
                    delta_y *= scale

                previous_cx, previous_cy = state.cx, state.cy
                state.cx += self.smoothing * delta_x
                state.cy += self.smoothing * delta_y
                state.width += self.size_smoothing * (measured_width - state.width)
                state.height += self.size_smoothing * (measured_height - state.height)
                state.velocity_x = state.velocity_x * 0.65 + (state.cx - previous_cx) * 0.35
                state.velocity_y = state.velocity_y * 0.65 + (state.cy - previous_cy) * 0.35
                state.template = detection.copy()
                state.missed = 0

            rendered = detection.copy()
            rendered["raw_box"] = raw_box(detection)
            rendered["stabilized"] = True
            rendered["held"] = False
            set_box(rendered, state.cx, state.cy, state.width, state.height, frame_width, frame_height)
            output.append(rendered)

        active_boxes = list(output)
        for track_id, state in list(self.states.items()):
            if track_id in seen:
                continue
            state.missed += 1
            if state.missed > self.hold_frames:
                del self.states[track_id]
                continue

            state.cx += state.velocity_x * 0.65
            state.cy += state.velocity_y * 0.65
            state.velocity_x *= 0.70
            state.velocity_y *= 0.70
            held = state.template.copy()
            held["raw_box"] = None
            held["stabilized"] = True
            held["held"] = True
            held["confidence"] = round(float(held.get("confidence", 0.0)) * (0.88 ** state.missed), 4)
            held["source"] = "temporal_body_hold"
            set_box(held, state.cx, state.cy, state.width, state.height, frame_width, frame_height)

            # ByteTrack can occasionally assign a new ID after a dropout. Do not
            # draw the held old ID over a live replacement occupying the same body.
            if any(box_iou(held, active) > 0.35 for active in active_boxes):
                continue
            output.append(held)

        return output


@dataclass
class FaceTrackState:
    template: dict[str, Any]
    relative_cx: float
    relative_cy: float
    relative_width: float
    relative_height: float
    missed: int = 0


class FaceBoxStabilizer:
    """Stabilize a face in body-relative coordinates for the same track ID.

    Keeping the face relative to its smoothed person box prevents the head lock
    from lagging behind when the whole person moves. It also lets a face survive a
    short YuNet miss while continuing to follow the body instead of freezing in
    screen coordinates.
    """

    def __init__(self, smoothing: float, hold_frames: int) -> None:
        self.smoothing = clamp(smoothing, 0.05, 1.0)
        self.size_smoothing = clamp(smoothing * 0.70, 0.05, 1.0)
        self.hold_frames = max(0, hold_frames)
        self.states: dict[int, FaceTrackState] = {}

    @staticmethod
    def relative_box(face: dict[str, Any], person: dict[str, Any]) -> tuple[float, float, float, float]:
        face_cx, face_cy, face_width, face_height = box_center_size(face)
        person_cx, person_cy, person_width, person_height = box_center_size(person)
        person_x1 = person_cx - person_width / 2.0
        person_y1 = person_cy - person_height / 2.0
        return (
            (face_cx - person_x1) / person_width,
            (face_cy - person_y1) / person_height,
            face_width / person_width,
            face_height / person_height,
        )

    @staticmethod
    def absolute_box(
        relative_cx: float,
        relative_cy: float,
        relative_width: float,
        relative_height: float,
        person: dict[str, Any],
    ) -> tuple[float, float, float, float]:
        person_cx, person_cy, person_width, person_height = box_center_size(person)
        person_x1 = person_cx - person_width / 2.0
        person_y1 = person_cy - person_height / 2.0
        return (
            person_x1 + relative_cx * person_width,
            person_y1 + relative_cy * person_height,
            relative_width * person_width,
            relative_height * person_height,
        )

    def update(
        self,
        detections: list[dict[str, Any]],
        people: list[dict[str, Any]],
        frame_width: int,
        frame_height: int,
    ) -> list[dict[str, Any]]:
        people_by_track = {
            int(person["track_id"]): person
            for person in people
            if person.get("track_id") is not None
        }
        output: list[dict[str, Any]] = []
        seen: set[int] = set()

        for detection in detections:
            track_id = detection.get("track_id")
            person = people_by_track.get(int(track_id)) if track_id is not None else None
            if track_id is None or person is None:
                passthrough = detection.copy()
                passthrough["raw_box"] = raw_box(detection)
                passthrough["stabilized"] = False
                passthrough["held"] = False
                output.append(passthrough)
                continue

            track_id = int(track_id)
            seen.add(track_id)
            measured = self.relative_box(detection, person)
            state = self.states.get(track_id)
            if state is None:
                state = FaceTrackState(detection.copy(), *measured)
                self.states[track_id] = state
            else:
                # Relative head movement should be small. Clamp a single YuNet
                # jump so one marginal detection cannot throw the lock across the body.
                delta_x = clamp(measured[0] - state.relative_cx, -0.12, 0.12)
                delta_y = clamp(measured[1] - state.relative_cy, -0.12, 0.12)
                state.relative_cx += self.smoothing * delta_x
                state.relative_cy += self.smoothing * delta_y
                state.relative_width += self.size_smoothing * (measured[2] - state.relative_width)
                state.relative_height += self.size_smoothing * (measured[3] - state.relative_height)
                state.template = detection.copy()
                state.missed = 0

            rendered = detection.copy()
            rendered["raw_box"] = raw_box(detection)
            rendered["stabilized"] = True
            rendered["held"] = False
            absolute = self.absolute_box(
                state.relative_cx,
                state.relative_cy,
                state.relative_width,
                state.relative_height,
                person,
            )
            set_box(rendered, *absolute, frame_width, frame_height)
            output.append(rendered)

        for track_id, state in list(self.states.items()):
            if track_id in seen:
                continue
            state.missed += 1
            person = people_by_track.get(track_id)
            if state.missed > self.hold_frames or person is None:
                del self.states[track_id]
                continue

            held = state.template.copy()
            held["raw_box"] = None
            held["stabilized"] = True
            held["held"] = True
            held["confidence"] = round(float(held.get("confidence", 0.0)) * (0.86 ** state.missed), 4)
            held["source"] = "temporal_face_hold"
            absolute = self.absolute_box(
                state.relative_cx,
                state.relative_cy,
                state.relative_width,
                state.relative_height,
                person,
            )
            set_box(held, *absolute, frame_width, frame_height)
            output.append(held)

        return output


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
            # Keep the event-level result as a fallback when the local hand
            # detector cannot see enough detail to recognize the gesture.
            return "minimal"
        return "normal"


def joint_angle(a: Any, b: Any, c: Any) -> float:
    first = (float(a.x) - float(b.x), float(a.y) - float(b.y), float(a.z) - float(b.z))
    second = (float(c.x) - float(b.x), float(c.y) - float(b.y), float(c.z) - float(b.z))
    denominator = math.sqrt(sum(value * value for value in first)) * math.sqrt(
        sum(value * value for value in second)
    )
    if denominator <= 1e-9:
        return 0.0
    cosine = clamp(sum(x * y for x, y in zip(first, second)) / denominator, -1.0, 1.0)
    return math.degrees(math.acos(cosine))


def is_middle_finger_gesture(landmarks: list[Any]) -> bool:
    """Orientation-independent middle-finger geometry from 21 hand landmarks."""

    if len(landmarks) < 21:
        return False

    def finger_angles(mcp: int, pip: int, dip: int, tip: int) -> tuple[float, float]:
        return (
            joint_angle(landmarks[mcp], landmarks[pip], landmarks[dip]),
            joint_angle(landmarks[pip], landmarks[dip], landmarks[tip]),
        )

    index = finger_angles(5, 6, 7, 8)
    middle = finger_angles(9, 10, 11, 12)
    ring = finger_angles(13, 14, 15, 16)
    pinky = finger_angles(17, 18, 19, 20)

    middle_extended = middle[0] >= 155.0 and middle[1] >= 155.0
    # Requiring every neighboring finger to be folded avoids calling a peace
    # sign (index + middle extended) an obscene gesture.
    others_folded = all(pip < 145.0 or dip < 145.0 for pip, dip in (index, ring, pinky))
    return middle_extended and others_folded


def distance_from_point_to_box(cx: float, cy: float, box: dict[str, Any]) -> float:
    dx = max(float(box["x1"]) - cx, 0.0, cx - float(box["x2"]))
    dy = max(float(box["y1"]) - cy, 0.0, cy - float(box["y2"]))
    return math.hypot(dx, dy)


class HandGestureDetector:
    """Detect a middle-finger gesture and associate it with a body track."""

    def __init__(self, confidence: float) -> None:
        self.enabled = False
        self.hands = None
        try:
            import mediapipe as mp

            self.hands = mp.solutions.hands.Hands(
                static_image_mode=False,
                max_num_hands=2,
                model_complexity=1,
                min_detection_confidence=clamp(confidence, 0.1, 0.9),
                min_tracking_confidence=clamp(confidence, 0.1, 0.9),
            )
            self.enabled = True
        except Exception as exc:  # noqa: BLE001 - optional sidecar capability.
            print(f"tracker: MediaPipe hand detector disabled: {exc}", file=sys.stderr)

    def detect(
        self,
        frame: Any,
        people: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        if not self.enabled or self.hands is None:
            return []

        import cv2

        height, width = frame.shape[:2]
        try:
            result = self.hands.process(cv2.cvtColor(frame, cv2.COLOR_BGR2RGB))
        except Exception as exc:  # noqa: BLE001 - degrade to event-level gesture context.
            print(f"tracker: MediaPipe hand detector failed; disabling it: {exc}", file=sys.stderr)
            self.enabled = False
            return []
        output: list[dict[str, Any]] = []
        handedness = result.multi_handedness or []
        for index, hand in enumerate(result.multi_hand_landmarks or []):
            landmarks = list(hand.landmark)
            if not is_middle_finger_gesture(landmarks):
                continue

            xs = [point.x * width for point in landmarks]
            ys = [point.y * height for point in landmarks]
            padding = max(8.0, 0.12 * max(max(xs) - min(xs), max(ys) - min(ys)))
            x1 = int(clamp(min(xs) - padding, 0, width - 2))
            y1 = int(clamp(min(ys) - padding, 0, height - 2))
            x2 = int(clamp(max(xs) + padding, x1 + 1, width - 1))
            y2 = int(clamp(max(ys) + padding, y1 + 1, height - 1))
            cx = (x1 + x2) / 2.0
            cy = (y1 + y2) / 2.0

            candidates: list[tuple[float, dict[str, Any]]] = []
            for person in people:
                if person.get("track_id") is None:
                    continue
                distance = distance_from_point_to_box(cx, cy, person)
                _, _, person_width, person_height = box_center_size(person)
                normalized_distance = distance / max(1.0, math.hypot(person_width, person_height))
                if normalized_distance <= 0.30:
                    candidates.append((normalized_distance, person))
            if not candidates:
                continue

            _, person = min(candidates, key=lambda item: item[0])
            confidence = 1.0
            if index < len(handedness) and handedness[index].classification:
                confidence = float(handedness[index].classification[0].score)
            output.append(
                {
                    "track_id": int(person["track_id"]),
                    "kind": "gesture",
                    "label": "middle finger",
                    "confidence": round(confidence, 4),
                    "x1": x1,
                    "y1": y1,
                    "x2": x2,
                    "y2": y2,
                    "source": "mediapipe_hands",
                    "raw_box": {"x1": x1, "y1": y1, "x2": x2, "y2": y2},
                    "stabilized": False,
                    "held": False,
                }
            )
        best_per_person: dict[int, dict[str, Any]] = {}
        for detection in output:
            track_id = int(detection["track_id"])
            existing = best_per_person.get(track_id)
            if existing is None or detection["confidence"] > existing["confidence"]:
                best_per_person[track_id] = detection
        return list(best_per_person.values())

    def close(self) -> None:
        if self.hands is not None:
            self.hands.close()


@dataclass
class GestureTrackState:
    hits: int = 0
    remaining: int = 0


class GestureLatch:
    """Debounce local gesture evidence and hold yellow per person track."""

    def __init__(self, confirmation_frames: int, hold_frames: int) -> None:
        self.confirmation_frames = max(1, confirmation_frames)
        self.hold_frames = max(0, hold_frames)
        self.states: dict[int, GestureTrackState] = {}

    def update(self, detections: list[dict[str, Any]]) -> set[int]:
        detected = {int(det["track_id"]) for det in detections if det.get("track_id") is not None}
        for track_id in detected:
            state = self.states.setdefault(track_id, GestureTrackState())
            state.hits += 1
            if state.hits >= self.confirmation_frames:
                state.remaining = self.hold_frames

        active: set[int] = set()
        for track_id, state in list(self.states.items()):
            if track_id not in detected:
                state.hits = 0
                if state.remaining > 0:
                    active.add(track_id)
                    state.remaining -= 1
            elif state.remaining > 0:
                active.add(track_id)
            if state.remaining == 0 and state.hits == 0 and track_id not in active:
                del self.states[track_id]
        return active


def color_for_kind(kind: str, frame_level: str) -> tuple[int, int, int]:
    # OpenCV uses BGR.
    if kind in {"knife", "object"}:
        return (0, 0, 255)
    if kind == "package":
        return (0, 255, 0)
    if kind == "gesture":
        return (0, 255, 255)
    if kind in {"face", "head", "person"} and frame_level == "threat":
        return (0, 0, 255)
    if kind in {"face", "head", "person"} and frame_level == "minimal":
        return (0, 255, 255)
    return (255, 255, 255)


def level_for_detection(
    detection: dict[str, Any],
    frame_level: str,
    gesture_track_ids: set[int],
) -> str:
    if frame_level == "threat":
        return "threat"
    track_id = detection.get("track_id")
    if detection["kind"] == "gesture" or (
        detection["kind"] in {"person", "face", "head"}
        and track_id is not None
        and int(track_id) in gesture_track_ids
    ):
        return "minimal"
    return frame_level


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


def choose_frame_rotation(video_path: Path, model_path: Path, threshold: float) -> int:
    """Choose one clip-wide rotation from face confidence on sampled frames."""

    import cv2

    if not model_path.is_file() or not hasattr(cv2, "FaceDetectorYN_create"):
        return 0
    capture = cv2.VideoCapture(str(video_path))
    frame_total = int(capture.get(cv2.CAP_PROP_FRAME_COUNT) or 0)
    if frame_total <= 0:
        capture.release()
        return 0

    detector = cv2.FaceDetectorYN_create(str(model_path), "", (320, 320), threshold, 0.3, 5000)
    scores = {rotation: 0.0 for rotation in (0, 90, 180, 270)}
    sample_indexes = sorted({int((frame_total - 1) * fraction) for fraction in (0.15, 0.35, 0.55, 0.75, 0.90)})
    for frame_index in sample_indexes:
        capture.set(cv2.CAP_PROP_POS_FRAMES, frame_index)
        ok, frame = capture.read()
        if not ok:
            continue
        for rotation in scores:
            rotated = rotate_frame(frame, rotation)
            height, width = rotated.shape[:2]
            detector.setInputSize((width, height))
            _, faces = detector.detect(rotated)
            if faces is not None:
                scores[rotation] += max(float(face[-1]) for face in faces)
    capture.release()

    best_rotation = max(scores, key=lambda rotation: (scores[rotation], rotation == 0))
    return best_rotation if scores[best_rotation] >= threshold else 0


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


def intersection_over_smaller(a: dict[str, Any], b: dict[str, Any]) -> float:
    x1 = max(a["x1"], b["x1"])
    y1 = max(a["y1"], b["y1"])
    x2 = min(a["x2"], b["x2"])
    y2 = min(a["y2"], b["y2"])
    intersection = max(0, x2 - x1) * max(0, y2 - y1)
    area_a = max(1, (a["x2"] - a["x1"]) * (a["y2"] - a["y1"]))
    area_b = max(1, (b["x2"] - b["x1"]) * (b["y2"] - b["y1"]))
    return intersection / min(area_a, area_b)


def suppress_nested_people(detections: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Prefer the broad body box when YOLO emits nested duplicate people."""

    ordered = sorted(
        detections,
        key=lambda det: (
            (det["x2"] - det["x1"]) * (det["y2"] - det["y1"]),
            det.get("confidence", 0.0),
        ),
        reverse=True,
    )
    kept: list[dict[str, Any]] = []
    for detection in ordered:
        if any(intersection_over_smaller(detection, existing) > 0.60 for existing in kept):
            continue
        kept.append(detection)
    return kept


def contains_center(outer: dict[str, Any], inner: dict[str, Any]) -> bool:
    cx = (inner["x1"] + inner["x2"]) / 2
    cy = (inner["y1"] + inner["y2"]) / 2
    return outer["x1"] <= cx <= outer["x2"] and outer["y1"] <= cy <= outer["y2"]


def plausible_face_for_person(person: dict[str, Any], face: dict[str, Any]) -> bool:
    """Reject rotation-induced YuNet false positives before they enter a track.

    Running YuNet over four rotations is useful for the physically rotated camera,
    but it can occasionally call a shoulder/torso-sized patch a face. A real face
    lock must be contained near the upper part of its person and remain reasonably
    small relative to that body box.
    """

    if not contains_center(person, face):
        return False
    _, face_cy, face_width, face_height = box_center_size(face)
    _, _, person_width, person_height = box_center_size(person)
    relative_center_y = (face_cy - float(person["y1"])) / person_height
    relative_area = (face_width * face_height) / (person_width * person_height)
    return (
        face_width / person_width <= 0.70
        and face_height / person_height <= 0.45
        and relative_area <= 0.25
        and relative_center_y <= 0.45
    )


def status_text_for(frame_level: str) -> str:
    return {
        "normal": "FRIDAY LOCK",
        "minimal": "FRIDAY LOCK - MINIMAL THREAT",
        "threat": "FRIDAY LOCK - THREAT",
    }[frame_level]


class FaceDetector:
    def __init__(self, model_path: Path, threshold: float, rotations: tuple[int, ...] = (0, 90, 180, 270)) -> None:
        import cv2

        self.enabled = model_path.is_file() and hasattr(cv2, "FaceDetectorYN_create")
        self.detector = None
        self.threshold = threshold
        self.rotations = rotations
        if self.enabled:
            self.detector = cv2.FaceDetectorYN_create(str(model_path), "", (320, 320), threshold, 0.3, 5000)
        else:
            print(f"tracker: face detector disabled; model/API unavailable: {model_path}", file=sys.stderr)

    def detect(self, frame: Any, person_detections: list[dict[str, Any]]) -> list[dict[str, Any]]:
        if not self.enabled or self.detector is None:
            return []

        height, width = frame.shape[:2]
        raw_faces: list[dict[str, Any]] = []
        for rotation in self.rotations:
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
                if plausible_face_for_person(person, face):
                    face["track_id"] = person.get("track_id")
                    break
            # Unassociated faces have no stable identity and were a major source
            # of one-frame boxes. Only render a face that belongs to a body track.
            if face["track_id"] is None:
                continue
            if face["track_id"] in used_person_tracks:
                continue
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
    input_width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH) or 640)
    input_height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT) or 480)
    cap.release()
    frame_rotation = choose_frame_rotation(Path(args.input), Path(args.face_model), args.face_conf)
    if frame_rotation in {90, 270}:
        width, height = input_height, input_width
    else:
        width, height = input_width, input_height
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
    face_detector = FaceDetector(Path(args.face_model), args.face_conf, rotations=(0,))
    gesture_detector = HandGestureDetector(args.hand_conf)
    names = model.names if isinstance(model.names, dict) else dict(enumerate(model.names))
    threat_latch = ThreatLatch(args.threat_hold_frames, args.threat_level, args.concerning_behavior)
    gesture_latch = GestureLatch(args.gesture_confirm_frames, args.gesture_hold_frames)
    body_stabilizer = BodyBoxStabilizer(args.body_smoothing, args.track_hold_frames)
    face_stabilizer = FaceBoxStabilizer(args.face_smoothing, args.track_hold_frames)
    gesture_box_stabilizer = FaceBoxStabilizer(args.face_smoothing, 0)
    pending: deque[dict[str, Any]] = deque()
    frames: list[dict[str, Any]] = []
    frame_count = 0
    detection_count = 0
    held_detection_count = 0
    gesture_frame_count = 0
    threat_preroll_frame_count = 0

    def render_record(record: dict[str, Any]) -> None:
        nonlocal detection_count, gesture_frame_count, held_detection_count, threat_preroll_frame_count
        frame = record["image"]
        frame_level = "threat" if record["forced_threat"] else record["base_level"]
        gesture_track_ids = record["gesture_track_ids"]
        gestures = [
            candidate
            for candidate in record["gesture_candidates"]
            if int(candidate["track_id"]) in gesture_track_ids
        ]
        detections = record["detections"] + gestures
        if gestures:
            gesture_frame_count += 1
        if record["forced_threat"] and record["base_level"] != "threat":
            threat_preroll_frame_count += 1
        detection_count += len(detections)
        held_detection_count += sum(bool(det.get("held")) for det in detections)

        for detection in detections:
            draw_box(
                frame,
                detection,
                level_for_detection(detection, frame_level, gesture_track_ids),
            )

        status_level = frame_level
        if status_level == "normal" and gesture_track_ids:
            status_level = "minimal"
        cv2.putText(
            frame,
            status_text_for(status_level),
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
                "frame_index": record["frame_index"],
                "event_threat_level": args.threat_level,
                "rendered_threat_level": status_level,
                "threat_preroll": record["forced_threat"] and record["base_level"] != "threat",
                "gesture_active_track_ids": sorted(gesture_track_ids),
                "detections": detections,
            }
        )

    source_capture = cv2.VideoCapture(str(args.input))
    frame_index = 0
    while True:
        ok, source_frame = source_capture.read()
        if not ok:
            break
        frame = rotate_frame(source_frame, frame_rotation)
        tracked = model.track(
            source=frame,
            persist=True,
            tracker=args.tracker,
            conf=args.conf,
            imgsz=max(320, args.imgsz),
            verbose=False,
        )
        if not tracked:
            frame_index += 1
            continue
        result = tracked[0]
        frame = result.orig_img.copy()
        other_detections: list[dict[str, Any]] = []
        raw_person_detections: list[dict[str, Any]] = []
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
                if kind == "person":
                    raw_person_detections.append(det)
                else:
                    other_detections.append(det)

        raw_person_detections = suppress_nested_people(raw_person_detections)
        person_detections = body_stabilizer.update(raw_person_detections, width, height)
        raw_face_detections = face_detector.detect(frame, person_detections)
        face_detections = face_stabilizer.update(raw_face_detections, person_detections, width, height)
        detections = other_detections + person_detections + face_detections

        raw_gesture_candidates = gesture_detector.detect(frame, person_detections)
        gesture_candidates = gesture_box_stabilizer.update(
            raw_gesture_candidates,
            person_detections,
            width,
            height,
        )
        gesture_track_ids = gesture_latch.update(gesture_candidates)
        base_level = threat_latch.update(detections)
        record = {
            "image": frame,
            "frame_index": frame_index,
            "base_level": base_level,
            "forced_threat": False,
            "gesture_track_ids": set(gesture_track_ids),
            "gesture_candidates": gesture_candidates,
            "detections": detections,
        }
        pending.append(record)

        # Confirmation arrives on the second candidate frame. Backfill the first
        # candidate while it is still in the short render buffer, so yellow starts
        # with the visible gesture rather than one frame late.
        for buffered in pending:
            candidate_tracks = {
                int(candidate["track_id"])
                for candidate in buffered["gesture_candidates"]
            }
            buffered["gesture_track_ids"].update(candidate_tracks & gesture_track_ids)

        # This is an offline rendered video, so use a small bounded buffer to
        # compensate for the generic COCO model recognizing a visible knife late.
        # Only person/face color is backfilled; no fake object box is synthesized.
        if any(det["kind"] in {"knife", "object"} for det in detections):
            for buffered in pending:
                buffered["forced_threat"] = True

        if len(pending) > max(0, args.threat_preroll_frames):
            render_record(pending.popleft())

        frame_count += 1
        frame_index += 1

    source_capture.release()
    gesture_detector.close()
    while pending:
        render_record(pending.popleft())
    writer.release()

    tracks = {
        "schema": "friday.tracks.v1",
        "source_video": str(args.input),
        "model": args.model,
        "tracker": args.tracker,
        "event_threat_level": args.threat_level,
        "render_policy": "gesture-and-fast-threat-lock-v5",
        "yolo_imgsz": args.imgsz,
        "threat_hold_frames": args.threat_hold_frames,
        "threat_preroll_frames": args.threat_preroll_frames,
        "threat_preroll_frame_count": threat_preroll_frame_count,
        "body_smoothing": args.body_smoothing,
        "face_smoothing": args.face_smoothing,
        "track_hold_frames": args.track_hold_frames,
        "hand_detector_enabled": gesture_detector.enabled,
        "gesture_confirm_frames": args.gesture_confirm_frames,
        "gesture_hold_frames": args.gesture_hold_frames,
        "gesture_frame_count": gesture_frame_count,
        "concerning_behavior": args.concerning_behavior,
        "fps": fps,
        "frame_rotation": frame_rotation,
        "input_width": input_width,
        "input_height": input_height,
        "width": width,
        "height": height,
        "frame_count": frame_count,
        "detection_count": detection_count,
        "held_detection_count": held_detection_count,
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
