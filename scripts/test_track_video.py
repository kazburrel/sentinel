import unittest
from types import SimpleNamespace

from scripts.track_video import (
    BodyBoxStabilizer,
    FaceBoxStabilizer,
    GestureLatch,
    ThreatLatch,
    is_middle_finger_gesture,
    level_for_detection,
    plausible_face_for_person,
    suppress_nested_people,
)


def detection(kind: str, track_id: int | None, x1: int, y1: int, x2: int, y2: int) -> dict:
    return {
        "track_id": track_id,
        "kind": kind,
        "label": kind,
        "confidence": 0.9,
        "x1": x1,
        "y1": y1,
        "x2": x2,
        "y2": y2,
        "source": "test",
    }


class BodyBoxStabilizerTests(unittest.TestCase):
    def test_suppresses_nested_duplicate_people_but_keeps_separate_people(self) -> None:
        broad = detection("person", 1, 20, 10, 300, 450)
        nested = detection("person", 2, 80, 90, 240, 380)
        separate = detection("person", 3, 400, 20, 600, 430)

        output = suppress_nested_people([nested, separate, broad])

        self.assertEqual({item["track_id"] for item in output}, {1, 3})

    def test_smooths_a_jittering_tracked_person(self) -> None:
        stabilizer = BodyBoxStabilizer(smoothing=0.25, hold_frames=2)
        first = stabilizer.update([detection("person", 7, 0, 0, 100, 200)], 640, 480)[0]
        second = stabilizer.update([detection("person", 7, 20, 0, 120, 200)], 640, 480)[0]

        self.assertEqual((first["x1"], first["x2"]), (0, 100))
        self.assertEqual((second["x1"], second["x2"]), (5, 105))
        self.assertEqual(second["raw_box"], {"x1": 20, "y1": 0, "x2": 120, "y2": 200})
        self.assertTrue(second["stabilized"])
        self.assertFalse(second["held"])

    def test_holds_brief_misses_then_expires(self) -> None:
        stabilizer = BodyBoxStabilizer(smoothing=0.30, hold_frames=2)
        stabilizer.update([detection("person", 3, 100, 40, 220, 300)], 640, 480)

        first_miss = stabilizer.update([], 640, 480)
        second_miss = stabilizer.update([], 640, 480)
        expired = stabilizer.update([], 640, 480)

        self.assertEqual(len(first_miss), 1)
        self.assertEqual(len(second_miss), 1)
        self.assertTrue(first_miss[0]["held"])
        self.assertIsNone(first_miss[0]["raw_box"])
        self.assertEqual(expired, [])

    def test_remaps_an_overlapping_replacement_id_to_the_existing_track(self) -> None:
        stabilizer = BodyBoxStabilizer(smoothing=0.30, hold_frames=3)
        stabilizer.update([detection("person", 1, 100, 40, 220, 300)], 640, 480)
        output = stabilizer.update([detection("person", 2, 102, 42, 222, 302)], 640, 480)

        self.assertEqual([item["track_id"] for item in output], [1])
        self.assertEqual(output[0]["source_track_id"], 2)


class FaceBoxStabilizerTests(unittest.TestCase):
    def test_rejects_a_torso_sized_false_face(self) -> None:
        person = detection("person", 5, 100, 50, 300, 400)
        real_face = detection("face", None, 155, 75, 235, 165)
        torso_patch = detection("face", None, 120, 110, 290, 310)

        self.assertTrue(plausible_face_for_person(person, real_face))
        self.assertFalse(plausible_face_for_person(person, torso_patch))

    def test_face_hold_follows_its_moving_body(self) -> None:
        stabilizer = FaceBoxStabilizer(smoothing=0.22, hold_frames=2)
        first_person = detection("person", 5, 100, 100, 300, 400)
        first_face = detection("face", 5, 150, 130, 230, 230)
        stabilizer.update([first_face], [first_person], 640, 480)

        moved_person = detection("person", 5, 140, 100, 340, 400)
        held = stabilizer.update([], [moved_person], 640, 480)[0]

        self.assertEqual((held["x1"], held["x2"]), (190, 270))
        self.assertTrue(held["held"])
        self.assertEqual(held["source"], "temporal_face_hold")

    def test_smooths_face_jitter_relative_to_body(self) -> None:
        stabilizer = FaceBoxStabilizer(smoothing=0.20, hold_frames=2)
        person = detection("person", 8, 100, 100, 300, 400)
        stabilizer.update([detection("face", 8, 150, 130, 230, 230)], [person], 640, 480)
        output = stabilizer.update(
            [detection("face", 8, 170, 130, 250, 230)],
            [person],
            640,
            480,
        )[0]

        self.assertEqual((output["x1"], output["x2"]), (154, 234))
        self.assertEqual(output["raw_box"]["x1"], 170)


class ThreatLatchTests(unittest.TestCase):
    def test_threat_hold_is_independent_of_box_stabilization(self) -> None:
        latch = ThreatLatch(hold_frames=2, event_threat_level="threat", concerning_behavior=False)
        concerning = [detection("object", None, 1, 1, 10, 10)]

        self.assertEqual(latch.update([]), "normal")
        self.assertEqual(latch.update(concerning), "threat")
        self.assertEqual(latch.update([]), "threat")
        self.assertEqual(latch.update([]), "threat")
        self.assertEqual(latch.update([]), "normal")


def point(x: float, y: float, z: float = 0.0) -> SimpleNamespace:
    return SimpleNamespace(x=x, y=y, z=z)


def synthetic_middle_finger() -> list[SimpleNamespace]:
    landmarks = [point(0, 0) for _ in range(21)]
    # Fold index, ring, and pinky at 90-degree joints.
    for mcp, pip, dip, tip, x in (
        (5, 6, 7, 8, 1.0),
        (13, 14, 15, 16, 3.0),
        (17, 18, 19, 20, 4.0),
    ):
        landmarks[mcp] = point(x, 0)
        landmarks[pip] = point(x, 1)
        landmarks[dip] = point(x + 1, 1)
        landmarks[tip] = point(x + 1, 0)
    # Keep only the middle finger straight.
    landmarks[9] = point(2, 0)
    landmarks[10] = point(2, 1)
    landmarks[11] = point(2, 2)
    landmarks[12] = point(2, 3)
    return landmarks


class GestureTests(unittest.TestCase):
    def test_recognizes_middle_finger_geometry_but_not_peace_sign(self) -> None:
        landmarks = synthetic_middle_finger()
        self.assertTrue(is_middle_finger_gesture(landmarks))

        # Extend the index too: this is now a two-finger/peace shape and must
        # not be treated as an obscene gesture.
        landmarks[5] = point(1, 0)
        landmarks[6] = point(1, 1)
        landmarks[7] = point(1, 2)
        landmarks[8] = point(1, 3)
        self.assertFalse(is_middle_finger_gesture(landmarks))

    def test_gesture_latch_confirms_then_holds_per_track(self) -> None:
        latch = GestureLatch(confirmation_frames=2, hold_frames=2)
        gesture = detection("gesture", 9, 10, 10, 40, 50)

        self.assertEqual(latch.update([gesture]), set())
        self.assertEqual(latch.update([gesture]), {9})
        self.assertEqual(latch.update([]), {9})
        self.assertEqual(latch.update([]), {9})
        self.assertEqual(latch.update([]), set())

    def test_red_overrides_gesture_yellow_for_the_same_person(self) -> None:
        person = detection("person", 9, 10, 10, 100, 200)

        self.assertEqual(level_for_detection(person, "normal", {9}), "minimal")
        self.assertEqual(level_for_detection(person, "threat", {9}), "threat")


if __name__ == "__main__":
    unittest.main()
