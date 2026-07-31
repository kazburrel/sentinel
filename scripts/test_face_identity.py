import unittest

from scripts.face_identity import best_profile_match, cosine_similarity, normalize


class FaceIdentityTests(unittest.TestCase):
    def test_normalize_and_cosine(self):
        left = normalize([3.0, 4.0])
        self.assertAlmostEqual(cosine_similarity(left, left), 1.0)
        self.assertAlmostEqual(cosine_similarity(left, normalize([-4.0, 3.0])), 0.0)

    def test_no_profiles_is_not_enrolled(self):
        result = best_profile_match([[1.0, 0.0]], [], 0.5, 0.65)
        self.assertEqual(result["status"], "not_enrolled")

    def test_two_regular_matches_confirm_identity(self):
        profiles = [{"person_id": "admin", "display_name": "Admin", "embeddings": [[1.0, 0.0]]}]
        queries = [normalize([0.8, 0.6]), normalize([0.7, 0.7])]
        result = best_profile_match(queries, profiles, 0.5, 0.95)
        self.assertEqual(result["status"], "known")
        self.assertEqual(result["known_person_id"], "admin")

    def test_one_strong_match_confirms_identity(self):
        profiles = [{"person_id": "admin", "display_name": "Admin", "embeddings": [[1.0, 0.0]]}]
        result = best_profile_match([normalize([0.9, 0.1])], profiles, 0.5, 0.65)
        self.assertEqual(result["status"], "known")

    def test_one_weak_match_stays_unknown(self):
        profiles = [{"person_id": "admin", "display_name": "Admin", "embeddings": [[1.0, 0.0]]}]
        result = best_profile_match([normalize([0.55, 0.835])], profiles, 0.5, 0.65)
        self.assertEqual(result["status"], "unknown")


if __name__ == "__main__":
    unittest.main()
