import unittest

from scripts.track_video import (
    BodyBoxStabilizer,
    FaceBoxStabilizer,
    ThreatLatch,
    plausible_face_for_person,
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

    def test_does_not_draw_stale_id_over_live_replacement(self) -> None:
        stabilizer = BodyBoxStabilizer(smoothing=0.30, hold_frames=3)
        stabilizer.update([detection("person", 1, 100, 40, 220, 300)], 640, 480)
        output = stabilizer.update([detection("person", 2, 102, 42, 222, 302)], 640, 480)

        self.assertEqual([item["track_id"] for item in output], [2])


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


if __name__ == "__main__":
    unittest.main()
