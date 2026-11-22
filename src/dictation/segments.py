"""OpenWhispr's pause-aware split plan, ported to PCM16 without dropping samples."""

from array import array
import math


def pause_cut(pcm, start, limit):
    frame_bytes = 320  # 10 ms at 16 kHz, two bytes per sample.
    samples = array("h")
    samples.frombytes(pcm[start:limit])
    energies = [sum(value * value for value in samples[offset:offset + 160])
                for offset in range(0, len(samples) - 159, 160)]
    if len(energies) < 10:
        return limit
    quietest = math.inf
    run_start = run_end = 0
    for offset in range(len(energies) - 9):
        energy = sum(energies[offset:offset + 10])
        if energy < quietest:
            quietest, run_start, run_end = energy, offset, offset
        elif energy == quietest and run_end == offset - 1:
            run_end = offset
    if quietest >= sum(energies[-10:]) * 0.5:
        return limit
    return start + (run_start + run_end + 10) // 2 * frame_bytes


def split_at_pauses(pcm):
    maximum = 15 * 16000 * 2
    allowance = 16000  # Let the final chunk absorb a tail of up to 0.5 seconds.
    count = math.ceil((len(pcm) - allowance) / maximum)
    start = emitted = 0
    while len(pcm) - start > maximum + allowance:
        limit = start + maximum
        after = count - emitted - 1
        earliest = max(limit - 4 * 16000 * 2, len(pcm) - after * maximum - allowance)
        cut = pause_cut(pcm, earliest, limit)
        yield pcm[start:cut]
        start, emitted = cut, emitted + 1
    if start < len(pcm):
        yield pcm[start:]
