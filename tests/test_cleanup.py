import unittest

from dictation.cleanup import corrections, process


class CleanupTests(unittest.TestCase):
    def test_fillers_and_surrounding_commas(self):
        self.assertEqual(process("Um, I, uh, think so.", {}), "I think so.")
        self.assertEqual(process("Um. Uh.", {}), "")

    def test_meaningful_words_negations_and_emphasis_remain(self):
        text = "I really like this, you know. I do not want to lose very very important words."
        self.assertEqual(process(text, {}), text)
        self.assertEqual(process("The plumber hummed; don't remove substrings.", {}),
                         "The plumber hummed; don't remove substrings.")

    def test_protected_urls_email_quotes_and_inline_code(self):
        text = 'Visit https://um.example, email uh@example.com and keep `um` or "uh" or \'erm\'.'
        self.assertEqual(process(text, {"corrections": "example => changed"}), text)

    def test_acronym_is_not_a_filler(self):
        self.assertEqual(process("A UM student said um hello.", {}), "A UM student said hello.")

    def test_corrections_are_literal_case_insensitive_and_do_not_cascade(self):
        rules = "open ai => OpenAI\nfoo => bar\nbar => baz\nsea plus plus => C++"
        self.assertEqual(process("OPEN AI, foo bar and sea plus plus.", {"corrections": rules}),
                         "OpenAI, bar baz and C++.")

    def test_longest_replacement_wins_and_vocabulary_case_is_applied(self):
        values = {"prompt": "OpenAI, niri", "corrections": "open ai tools => the SDK\nopen ai => OpenAI"}
        self.assertEqual(process("Niri uses open ai tools and openai.", values), "niri uses the SDK and OpenAI.")

    def test_filters_match_complete_phrases_and_tokens(self):
        values = {"blocked_words": "you know, like, can"}
        self.assertEqual(process("I, you know, like my canary and can't change like-minded people.", values),
                         "I my canary and can't change like-minded people.")

    def test_every_cleanup_feature_can_be_disabled(self):
        text = "Um, open ai and the the thing."
        values = {"auto_correct": False, "filter_words": False, "remove_repetitions": False,
                  "corrections": "open ai => OpenAI"}
        self.assertEqual(process(text, values), text)
        self.assertEqual(process("...", values), "...")

    def test_repetitions_are_only_removed_when_enabled(self):
        self.assertEqual(process("the the thing", {}), "the the thing")
        self.assertEqual(process("the the thing", {"remove_repetitions": True}), "the thing")

    def test_correction_validation(self):
        for rules in ("invalid", "word => ", " => word", "one => a\nONE => b", ". => end"):
            with self.subTest(rules=rules), self.assertRaises(ValueError):
                corrections(rules)
