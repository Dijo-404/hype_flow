const SECOND: usize = 16_000 * 2;
const MAXIMUM: usize = 15 * SECOND;
const ALLOWANCE: usize = SECOND / 2;
const FRAME: usize = SECOND / 100;
const PAUSE_FRAMES: usize = 10;

fn pause_cut(pcm: &[u8], start: usize, limit: usize) -> usize {
    let energies: Vec<u64> = pcm[start..limit]
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(|frame| {
            frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| {
                    let sample = i64::from(i16::from_le_bytes(*sample));
                    (sample * sample) as u64
                })
                .sum()
        })
        .collect();
    if energies.len() < PAUSE_FRAMES {
        return limit;
    }
    let mut quietest = u64::MAX;
    let (mut run_start, mut run_end) = (0, 0);
    for (offset, window) in energies.windows(PAUSE_FRAMES).enumerate() {
        let energy = window.iter().sum::<u64>();
        if energy < quietest {
            quietest = energy;
            (run_start, run_end) = (offset, offset);
        } else if energy == quietest && run_end + 1 == offset {
            run_end = offset;
        }
    }
    if 2 * quietest
        >= energies[energies.len() - PAUSE_FRAMES..]
            .iter()
            .sum::<u64>()
    {
        return limit;
    }
    start + (run_start + run_end + PAUSE_FRAMES) / 2 * FRAME
}

pub fn split_at_pauses(pcm: &[u8]) -> Vec<&[u8]> {
    let count = pcm.len().saturating_sub(ALLOWANCE).div_ceil(MAXIMUM);
    let mut slices = Vec::with_capacity(count);
    let mut start = 0;
    while pcm.len() - start > MAXIMUM + ALLOWANCE {
        let limit = start + MAXIMUM;
        let after = count - slices.len() - 1;
        let earliest = (limit - 4 * SECOND).max(pcm.len() - after * MAXIMUM - ALLOWANCE);
        let cut = pause_cut(pcm, earliest, limit);
        slices.push(&pcm[start..cut]);
        start = cut;
    }
    if start < pcm.len() {
        slices.push(&pcm[start..]);
    }
    slices
}
