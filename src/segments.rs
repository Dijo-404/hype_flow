use std::borrow::Cow;

const SECOND: usize = 16_000 * 2;
const MAXIMUM: usize = 15 * SECOND;
const ALLOWANCE: usize = SECOND / 2;
const FRAME: usize = SECOND / 100;
const PAUSE_FRAMES: usize = 10;
// One Silero VAD window of mono PCM16.
const CTC_PREROLL: usize = 512 * 2;

fn leading_silence(pcm: &[u8]) -> usize {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .take_while(|sample| **sample == [0, 0])
        .count()
        * 2
}

/// Give CTC the same VAD window and split alignment regardless of digital silence.
pub(crate) fn ctc_preroll(pcm: &[u8]) -> (Cow<'_, [u8]>, isize) {
    let silence = leading_silence(pcm);
    let offset = silence as isize - CTC_PREROLL as isize;
    if offset >= 0 {
        return (Cow::Borrowed(&pcm[offset as usize..]), offset);
    }
    let mut padded = Vec::with_capacity(pcm.len() + offset.unsigned_abs());
    padded.resize(CTC_PREROLL, 0);
    padded.extend_from_slice(&pcm[silence..]);
    (Cow::Owned(padded), offset)
}

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

/// Bound every decode, including audio without VAD, and prefer quiet cut points.
pub fn split_at_pauses_with_limit(pcm: &[u8], maximum_seconds: usize) -> Vec<&[u8]> {
    let maximum = maximum_seconds.max(5) * SECOND;
    let mut remaining = pcm;
    let mut slices = Vec::new();
    while remaining.len() > maximum {
        let cut = pause_cut(remaining, maximum.saturating_sub(4 * SECOND), maximum)
            .min(remaining.len().saturating_sub(SECOND));
        slices.push(&remaining[..cut]);
        remaining = &remaining[cut..];
    }
    if !remaining.is_empty() {
        slices.push(remaining);
    }
    slices
}

/// Commit at pauses after five seconds, or at the bounded decode limit.
pub(crate) fn eager_cut(pcm: &[u8], maximum_seconds: usize, ctc: bool) -> Option<usize> {
    let maximum = maximum_seconds.max(5) * SECOND;
    let mut padding = 0;
    if ctc && pcm.len() + CTC_PREROLL >= 5 * SECOND {
        let silence = leading_silence(pcm);
        if silence > CTC_PREROLL {
            // Send the removed silence separately so its duration still counts.
            return Some((silence - CTC_PREROLL).min(maximum));
        }
        // Match the decoder's preroll without copying PCM on the capture thread.
        padding = CTC_PREROLL - silence;
    }
    let maximum = maximum - padding;
    if pcm.len() >= maximum + SECOND / 2 {
        return Some(pause_cut(pcm, maximum.saturating_sub(4 * SECOND), maximum));
    }
    const QUIET: usize = SECOND * 3 / 5;
    if pcm.len() + padding >= 5 * SECOND + QUIET {
        let tail = &pcm[pcm.len() - QUIET..];
        let sum: f64 = tail
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| {
                let sample = f64::from(i16::from_le_bytes(*sample)) / 32768.0;
                sample * sample
            })
            .sum();
        if (sum / (QUIET / 2) as f64).sqrt() < 0.003 {
            return Some((pcm.len() - QUIET / 2).min(maximum));
        }
    }
    None
}

#[cfg(test)]
mod bounded_tests {
    use super::*;

    #[test]
    fn bounded_cuts_preserve_every_sample_and_prefer_silence() {
        let mut pcm = vec![0x7f; 65 * SECOND];
        pcm[17 * SECOND..18 * SECOND].fill(0);
        let pieces = split_at_pauses_with_limit(&pcm, 20);
        assert!(pieces.iter().all(|piece| piece.len() <= 20 * SECOND));
        assert!(pieces[0].len() >= 17 * SECOND && pieces[0].len() <= 18 * SECOND);
        assert_eq!(pieces.concat(), pcm);
    }

    #[test]
    fn eager_commit_needs_minimum_speech_time_or_maximum() {
        assert_eq!(eager_cut(&vec![0; 4 * SECOND], 20, false), None);
        assert!(eager_cut(&vec![0; 6 * SECOND], 20, false).is_some());
        assert_eq!(eager_cut(&vec![0x7f; 19 * SECOND], 20, false), None);
        assert_eq!(eager_cut(&vec![0x7f; 20 * SECOND], 20, false), None);
        assert_eq!(
            eager_cut(&vec![0x7f; 20 * SECOND + SECOND / 2], 20, false),
            Some(20 * SECOND)
        );
    }

    #[test]
    fn eager_ctc_separates_excess_digital_silence_before_speech() {
        let mut pcm = vec![0; 3 * SECOND];
        // The low byte is zero, but this sample is audible PCM.
        pcm.extend([0, 1].repeat(3 * SECOND / 2));
        let prefix = 3 * SECOND - CTC_PREROLL;
        assert_eq!(eager_cut(&pcm, 15, true), Some(prefix));
        assert_eq!(eager_cut(&pcm, 15, false), None);
        assert_eq!(ctc_preroll(&pcm[prefix..]).1, 0);
        assert_eq!(eager_cut(&pcm[prefix..], 15, true), None);
        assert_eq!(
            eager_cut(&vec![0; 20 * SECOND], 15, true),
            Some(15 * SECOND)
        );
    }

    #[test]
    fn ctc_preroll_produces_identical_pcm_and_cut_positions() {
        let voice = [0, 1].repeat(16 * SECOND / 2);
        let mut delayed = vec![0; 3 * SECOND];
        delayed.extend_from_slice(&voice);
        let (base, base_offset) = ctc_preroll(&voice);
        let (padded, padded_offset) = ctc_preroll(&delayed);
        assert_eq!(base, padded);
        assert_eq!(padded_offset - base_offset, (3 * SECOND) as isize);
        let skip = eager_cut(&delayed, 15, true).unwrap();
        let baseline_cut = eager_cut(&voice, 15, true).unwrap();
        let delayed_cut = eager_cut(&delayed[skip..], 15, true).unwrap();
        assert_eq!(baseline_cut + CTC_PREROLL, delayed_cut);
        assert_eq!(&voice[baseline_cut..], &delayed[skip + delayed_cut..]);
    }
    #[test]
    fn short_final_tails_are_rebalanced_without_exceeding_limit() {
        for extra in [SECOND / 10, SECOND / 2, SECOND / 2 + 2] {
            let pcm = vec![0x7f; 15 * SECOND + extra];
            let pieces = split_at_pauses_with_limit(&pcm, 15);
            assert!(
                pieces
                    .iter()
                    .all(|piece| piece.len() >= SECOND && piece.len() <= 15 * SECOND)
            );
            assert_eq!(pieces.concat(), pcm);
            assert_eq!(
                eager_cut(&pcm[..15 * SECOND + SECOND / 10], 15, false),
                None
            );
        }
    }
}
