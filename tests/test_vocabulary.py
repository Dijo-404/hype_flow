from pathlib import Path
import struct
import tempfile
import unittest

from dictation.vocabulary import hotwords, sentencepiece_vocabulary, terms


class VocabularyTests(unittest.TestCase):
    def test_deduplication_limits_and_native_score_injection(self):
        self.assertEqual(terms("niri, OpenAI\nNIRI"), ["niri", "OpenAI"])
        self.assertEqual(hotwords("niri"), "niri\nNiri")
        with self.assertRaises(ValueError):
            hotwords("smile :999")
        with self.assertRaises(ValueError):
            terms(",".join("word" + str(index) for index in range(101)))
        with self.assertRaises(ValueError):
            terms("bad\x00word")

    def test_case_variants_never_exceed_native_hotword_cap(self):
        result = hotwords(",".join("word" + str(index) for index in range(100)))
        self.assertEqual(len(result.splitlines()), 100)
        self.assertEqual(len(set(result.splitlines())), 100)
        with self.assertRaises(ValueError):
            hotwords(",".join("word" + str(index) for index in range(100)) + ",bad :99")

    def test_sentencepiece_export_keeps_original_scores_and_checks_tokens(self):
        # A minimal ModelProto with one SentencePiece message: piece='a', score=-1.0.
        piece = b"\x0a\x01a\x15" + struct.pack("<f", -1.0)
        model = b"\x0a" + bytes([len(piece)]) + piece
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tokens.txt"
            path.write_text("a 0\n<blk> 1\n")
            self.assertEqual(sentencepiece_vocabulary(model, path), "a\t-1\n")
            path.write_text("b 0\n<blk> 1\n")
            with self.assertRaisesRegex(ValueError, "does not match"):
                sentencepiece_vocabulary(model, path)

    def test_malformed_tokenizer_cannot_overread(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tokens.txt"
            path.write_text("a 0\n")
            for data in (b"\x0a\xff", b"\x0a\x05x", b"\x00", b"\x0b"):
                with self.subTest(data=data), self.assertRaises(ValueError):
                    sentencepiece_vocabulary(data, path)
