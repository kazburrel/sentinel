#!/usr/bin/env python3
"""Local face enrollment and recognition for Project FRIDAY.

YuNet detects and aligns a face in each supplied event image. SFace turns the
aligned crop into a normalized embedding. Enrollment stores embeddings only;
the source photos are never copied into the identity profile. Recognition
prints one small JSON object for the Rust server to consume.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import sys
import time
from pathlib import Path
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FACE_MODEL = REPO_ROOT / "scripts" / "models" / "face_detection_yunet_2023mar.onnx"
DEFAULT_RECOGNITION_MODEL = REPO_ROOT / "scripts" / "models" / "face_recognition_sface_2021dec.onnx"
PROFILE_VERSION = 1
DEFAULT_MATCH_THRESHOLD = 0.50
DEFAULT_STRONG_MATCH_THRESHOLD = 0.65


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Enroll or recognize a local FRIDAY identity")
    parser.add_argument("--face-model", default=str(DEFAULT_FACE_MODEL), type=Path)
    parser.add_argument("--recognition-model", default=str(DEFAULT_RECOGNITION_MODEL), type=Path)
    parser.add_argument("--face-conf", default=0.45, type=float)
    subparsers = parser.add_subparsers(dest="command", required=True)

    enroll = subparsers.add_parser("enroll", help="Create a local embedding profile")
    enroll.add_argument("--person-id", required=True)
    enroll.add_argument("--display-name", required=True)
    enroll.add_argument("--output", required=True, type=Path)
    enroll.add_argument("images", nargs="+", type=Path)

    recognize = subparsers.add_parser("recognize", help="Match event images against local profiles")
    recognize.add_argument("--profiles-dir", required=True, type=Path)
    recognize.add_argument("--threshold", default=DEFAULT_MATCH_THRESHOLD, type=float)
    recognize.add_argument("--strong-threshold", default=DEFAULT_STRONG_MATCH_THRESHOLD, type=float)
    recognize.add_argument("images", nargs="+", type=Path)
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


def normalize(values: Iterable[float]) -> list[float]:
    vector = [float(value) for value in values]
    magnitude = math.sqrt(sum(value * value for value in vector))
    if not vector or magnitude <= 1e-12:
        raise ValueError("empty face embedding")
    return [value / magnitude for value in vector]


def cosine_similarity(left: list[float], right: list[float]) -> float:
    if len(left) != len(right) or not left:
        raise ValueError("face embeddings have incompatible dimensions")
    return sum(a * b for a, b in zip(left, right, strict=True))


def best_detected_face(frame: Any, detector: Any, threshold: float) -> tuple[Any, Any, int] | None:
    best: tuple[float, Any, Any, int] | None = None
    for rotation in (0, 90, 180, 270):
        rotated = rotate_frame(frame, rotation)
        height, width = rotated.shape[:2]
        detector.setInputSize((width, height))
        _, faces = detector.detect(rotated)
        if faces is None:
            continue
        usable_faces = []
        for face in faces:
            x, y, w, h = [float(value) for value in face[:4]]
            confidence = float(face[-1])
            if confidence < threshold or w <= 1 or h <= 1:
                continue
            usable_faces.append(face)
            area_ratio = max(0.0, min(1.0, (w * h) / max(1.0, width * height)))
            clipped = x <= 2 or y <= 2 or x + w >= width - 2 or y + h >= height - 2
            quality = confidence * (1.0 + area_ratio * 8.0) * (0.75 if clipped else 1.0)
            if best is None or quality > best[0]:
                best = (quality, rotated, face, len(usable_faces))
        if best is not None and best[1] is rotated:
            best = (best[0], best[1], best[2], len(usable_faces))
    return None if best is None else (best[1], best[2], best[3])


def embedding_for_image(
    image_path: Path, detector: Any, recognizer: Any, threshold: float
) -> tuple[list[float], int] | None:
    import cv2

    frame = cv2.imread(str(image_path))
    if frame is None:
        print(f"identity: unreadable image skipped: {image_path}", file=sys.stderr)
        return None
    detected = best_detected_face(frame, detector, threshold)
    if detected is None:
        print(f"identity: no usable face in {image_path}", file=sys.stderr)
        return None
    rotated, face, face_count = detected
    aligned = recognizer.alignCrop(rotated, face)
    feature = recognizer.feature(aligned).reshape(-1)
    return normalize(float(value) for value in feature), face_count


def load_models(face_model: Path, recognition_model: Path, face_conf: float) -> tuple[Any, Any]:
    try:
        import cv2
    except Exception as exc:  # noqa: BLE001
        raise RuntimeError(f"OpenCV unavailable: {exc}") from exc

    if not face_model.is_file():
        raise RuntimeError(f"face detector model missing: {face_model}")
    if not recognition_model.is_file():
        raise RuntimeError(f"face recognition model missing: {recognition_model}")
    if not hasattr(cv2, "FaceDetectorYN_create") or not hasattr(cv2, "FaceRecognizerSF_create"):
        raise RuntimeError("installed OpenCV lacks YuNet/SFace support")

    detector = cv2.FaceDetectorYN_create(str(face_model), "", (320, 320), face_conf, 0.3, 5000)
    recognizer = cv2.FaceRecognizerSF_create(str(recognition_model), "")
    return detector, recognizer


def extract_embeddings(
    images: Iterable[Path], detector: Any, recognizer: Any, threshold: float
) -> tuple[list[list[float]], bool]:
    embeddings: list[list[float]] = []
    multiple_faces = False
    for image in images:
        if not image.is_file():
            print(f"identity: missing image skipped: {image}", file=sys.stderr)
            continue
        result = embedding_for_image(image, detector, recognizer, threshold)
        if result is not None:
            embedding, face_count = result
            embeddings.append(embedding)
            multiple_faces = multiple_faces or face_count > 1
    return embeddings, multiple_faces


def model_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as model_file:
        for chunk in iter(lambda: model_file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_private_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        path.parent.chmod(0o700)
    except OSError:
        pass
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    try:
        temporary.chmod(0o600)
    except OSError:
        pass
    temporary.replace(path)


def load_profiles(directory: Path, expected_model_sha256: str) -> list[dict[str, Any]]:
    profiles: list[dict[str, Any]] = []
    if not directory.is_dir():
        return profiles
    for path in sorted(directory.glob("*.json")):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
            if payload.get("version") != PROFILE_VERSION:
                raise ValueError("unsupported profile version")
            if payload.get("model_sha256") != expected_model_sha256:
                raise ValueError("profile was created with a different recognition model")
            embeddings = [normalize(values) for values in payload["embeddings"]]
            if not embeddings:
                raise ValueError("profile has no embeddings")
            profiles.append(
                {
                    "person_id": str(payload["person_id"]),
                    "display_name": str(payload["display_name"]),
                    "embeddings": embeddings,
                }
            )
        except (KeyError, TypeError, ValueError, json.JSONDecodeError, OSError) as exc:
            print(f"identity: invalid profile skipped: {path}: {exc}", file=sys.stderr)
    return profiles


def best_profile_match(
    query_embeddings: list[list[float]],
    profiles: list[dict[str, Any]],
    threshold: float,
    strong_threshold: float,
) -> dict[str, Any]:
    if not profiles:
        return {"status": "not_enrolled", "known_person_id": None, "display_name": None, "confidence": None}
    if not query_embeddings:
        return {"status": "no_face", "known_person_id": None, "display_name": None, "confidence": None}

    best_candidate: tuple[int, float, float, dict[str, Any]] | None = None
    best_unknown_score = -1.0
    for profile in profiles:
        per_image_scores = [
            max(cosine_similarity(query, reference) for reference in profile["embeddings"])
            for query in query_embeddings
        ]
        per_image_scores.sort(reverse=True)
        best_unknown_score = max(best_unknown_score, per_image_scores[0])
        supporting = [score for score in per_image_scores if score >= threshold]
        support_count = len(supporting)
        confidence = sum(supporting[:2]) / min(2, support_count) if supporting else per_image_scores[0]
        candidate = (support_count, confidence, per_image_scores[0], profile)
        if best_candidate is None or candidate[:3] > best_candidate[:3]:
            best_candidate = candidate

    assert best_candidate is not None
    support_count, confidence, strongest, profile = best_candidate
    confirmed = support_count >= 2 or strongest >= strong_threshold
    if confirmed:
        return {
            "status": "known",
            "known_person_id": profile["person_id"],
            "display_name": profile["display_name"],
            "confidence": round(max(confidence, strongest if support_count == 1 else confidence), 4),
        }
    return {
        "status": "unknown",
        "known_person_id": None,
        "display_name": None,
        "confidence": round(best_unknown_score, 4),
    }


def enroll(args: argparse.Namespace, detector: Any, recognizer: Any) -> int:
    embeddings, multiple_faces = extract_embeddings(args.images, detector, recognizer, args.face_conf)
    if multiple_faces:
        print("identity: enrollment images must contain only the person being enrolled", file=sys.stderr)
        return 5
    if len(embeddings) < 2:
        print("identity: enrollment needs usable faces from at least two images", file=sys.stderr)
        return 4
    payload = {
        "version": PROFILE_VERSION,
        "person_id": args.person_id,
        "display_name": args.display_name,
        "model": args.recognition_model.name,
        "model_sha256": model_digest(args.recognition_model),
        "created_at_unix": int(time.time()),
        "embedding_count": len(embeddings),
        "embeddings": embeddings,
    }
    write_private_json(args.output, payload)
    print(
        json.dumps(
            {
                "status": "enrolled",
                "person_id": args.person_id,
                "display_name": args.display_name,
                "embedding_count": len(embeddings),
            }
        )
    )
    return 0


def recognize(args: argparse.Namespace, detector: Any, recognizer: Any) -> int:
    if not 0.0 <= args.threshold <= 1.0:
        raise ValueError("match threshold must be between 0 and 1")
    if not args.threshold <= args.strong_threshold <= 1.0:
        raise ValueError("strong threshold must be between match threshold and 1")
    profiles = load_profiles(args.profiles_dir, model_digest(args.recognition_model))
    if not profiles:
        print(json.dumps(best_profile_match([], [], args.threshold, args.strong_threshold)))
        return 0
    embeddings, multiple_faces = extract_embeddings(args.images, detector, recognizer, args.face_conf)
    if multiple_faces:
        print(
            json.dumps(
                {
                    "status": "multiple_faces",
                    "known_person_id": None,
                    "display_name": None,
                    "confidence": None,
                }
            )
        )
        return 0
    print(json.dumps(best_profile_match(embeddings, profiles, args.threshold, args.strong_threshold)))
    return 0


def main() -> int:
    args = parse_args()
    try:
        detector, recognizer = load_models(args.face_model, args.recognition_model, args.face_conf)
        if args.command == "enroll":
            return enroll(args, detector, recognizer)
        return recognize(args, detector, recognizer)
    except Exception as exc:  # noqa: BLE001
        print(f"identity: failed: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
