import unittest

from tools.evaluate import edits, words


class EvaluationTests(unittest.TestCase):
    def test_insert_delete_substitute(self):
        self.assertEqual(edits("a b c".split(), "a x c d".split()), 2)
        self.assertEqual(edits(["a"], []), 1)
        self.assertEqual(edits([], ["a"]), 1)
        self.assertEqual(edits([], []), 0)

    def test_unicode_normalization_keeps_indic_marks(self):
        self.assertEqual(words("नमस्ते, दुनिया!"), ["नमस्ते", "दुनिया"])
        self.assertEqual(words("Hello, WORLD!"), ["hello", "world"])


if __name__ == "__main__":
    unittest.main()
