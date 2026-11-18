import os, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from metrics import budget_coverage, file_prf


class FilePrf(unittest.TestCase):
    def test_basic(self):
        p, r, f = file_prf(["a", "b", "c", "d"], ["a", "x"])
        self.assertAlmostEqual(p, 0.25)
        self.assertAlmostEqual(r, 0.5)
        self.assertAlmostEqual(f, 2 * 0.25 * 0.5 / 0.75)

    def test_perfect(self):
        self.assertEqual(file_prf(["a", "b"], ["b", "a"]), (1.0, 1.0, 1.0))

    def test_empty_returned(self):
        self.assertEqual(file_prf([], ["a"]), (0.0, 0.0, 0.0))

    def test_no_overlap(self):
        self.assertEqual(file_prf(["z"], ["a"]), (0.0, 0.0, 0.0))

    def test_empty_gold(self):
        self.assertEqual(file_prf(["a"], []), (0.0, 0.0, 0.0))

    def test_duplicates_ignored(self):
        self.assertEqual(file_prf(["a", "a"], ["a"]), (1.0, 1.0, 1.0))


class Coverage(unittest.TestCase):
    def test_partial(self):
        self.assertAlmostEqual(budget_coverage({"a": 50, "b": 100}, {"a": 100, "b": 100}), 0.75)

    def test_capped_at_size(self):
        self.assertAlmostEqual(budget_coverage({"a": 999}, {"a": 100}), 1.0)

    def test_missing_and_empty(self):
        self.assertEqual(budget_coverage({}, {"a": 10}), 0.0)
        self.assertEqual(budget_coverage({"a": 5}, {}), 0.0)

    def test_non_gold_ignored(self):
        self.assertEqual(budget_coverage({"z": 100}, {"a": 10}), 0.0)


if __name__ == "__main__":
    unittest.main()
