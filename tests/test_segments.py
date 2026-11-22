from array import array
import unittest

from dictation.segments import split_at_pauses


class SegmentTests(unittest.TestCase):
    def test_every_sample_is_preserved(self):
        pcm = array("h", range(-100, 100)).tobytes() * 5000
        segments = list(split_at_pauses(pcm))
        self.assertEqual(b"".join(segments), pcm)
        self.assertTrue(all(len(segment) <= 15.5 * 16000 * 2 for segment in segments))

    def test_quiet_pause_avoids_fixed_mid_word_cut(self):
        second = array("h", [1000] * 16000).tobytes()
        pcm = second * 12 + b"\0\0" * 8000 + second * 8
        segments = list(split_at_pauses(pcm))
        self.assertEqual(len(segments), 2)
        self.assertTrue(12 * len(second) < len(segments[0]) < 12.5 * len(second))
        self.assertEqual(b"".join(segments), pcm)

    def test_short_tail_is_not_decoded_alone(self):
        pcm = b"\x01\x00" * int(15.25 * 16000)
        self.assertEqual(list(split_at_pauses(pcm)), [pcm])
