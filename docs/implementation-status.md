# Accuracy, latency and memory implementation status

Updated: 2026-11-24. This records the implementation of
[accuracy-latency-memory-plan.md](accuracy-latency-memory-plan.md) and
[reference-comparison.md](reference-comparison.md). The original recommendations
remain intact; this document distinguishes implemented code, evidence, open gates
and work that still needs a representative corpus or a separate product change.

Verification uses staged builds and isolated desktop sessions. The live installation is unchanged.

## Implemented

| Plan item | Reference item | Result |
|---|---|---|
| Step 0, A8 | 9 | Pinned TOML configuration, strict/Whisper/formatted raw and cleaned WER, S/I/D counts, vocabulary precision/recall/F1, tags, length buckets, processing p50/p95, minimum-of-N repeats, startup time, audio/processing ratio and worker `VmHWM`. JSONL cases and aggregate summary JSON remain available. |
| Step 0 datasets | 9 | Reproducible external corpus preparation: seven Open ASR datasets, immutable dataset revision, fixed seed, resumable downloads, noise/boundary-silence augmentation, whole-clip long-form stitching with checkpoints, and no-vocabulary controls. Personal audio is not collected automatically. |
| A1 | 1 | Conservative, unambiguous fuzzy matching; exact short terms; common-word list; protected addresses/paths; true-positive fixtures and false-positive regressions in Rust and Python. Fuzzy correction now defaults off; existing explicit preferences remain respected. |
| A5 | — | Opt-in capped gain normalization before VAD, prompted by the quiet-input measurement below; the default remains off. |
| A3 safeguards | 11 | Generated hotword entries are capped at 100, including case variants. Boosting remains opt-in; no winning score is claimed. |
| A4 | 2 | Strip `<unk>`/known noise tags; reject punctuation-only output and dictionary echoes when boosting is enabled. Preserve intentional doubles such as “had had”, “very very” and “no no”. |
| A2, M1 | 6, 11 | Configurable quiet-window cuts bound both app requests and worker decodes. Production and pinned evaluation defaults use 15 seconds; earlier exploratory runs used 20 seconds. Noncontiguous padded VAD ranges receive a 100 ms zero gap. |
| L1 | 12 | Decode completed pieces during capture through a bounded queue using the existing worker request protocol. Keep ordered text and a final tail; preserve cancellation epochs and retain pending audio when the eager queue is full. |
| L2 | 15 | Opt-in modifier-release polling; unreadable keyboards retain the existing 180 ms fallback. No permission/group changes. Existing safe selection restoration and focus checks remain in place. |
| M2 | — | Idle unload adds 1 minute, 2 minutes and “after each dictation”. |
| — | 2 | App ignores recordings shorter than 300 ms. With VAD enabled, no detected speech returns empty text instead of decoding all input. |
| — | 3, 4 | Explicit `nemo_transducer` model type and an app-requested warm-up decode before ready. |
| — | 5 | Worker calls allocator trimming after inference. Global allocator policy and app-side trimming are not enabled without measurement. |
| — | 6 | VAD buffer shrinks from 300 to 65 seconds, paired with a 60-second maximum speech segment. A 40-second buffer with a 60-second maximum would be inconsistent. |
| — | 7 | Stop capture before playing the stop cue. |
| A8 | 9 | Model-free CI workflow plus opt-in real-worker invariance/golden/checkpoint runner. Exact transcript invariance remains a failing acoustic gate on some cases below. |

## Deliberate differences from the proposals

The published Whisper English normalizer is embedded from pinned upstream
MIT-licensed source and invoked through an evaluation-only Python standard-library
helper. This preserves its complete number/contraction/spelling grammar instead
of shipping an approximate Rust port. Dictation and inference remain native
Rust. Scoring/Python startup is outside reported processing latency. Provenance
and license are in [third_party.md](third_party.md).

The existing framed PCM worker protocol is reused for eager decoding rather
than introducing chunk/end messages. Each completed bounded piece is already
a valid request. This retains the established cancellation and process lifecycle.

The fuzzy rules reconcile conflicting recommendations: exact short terms are
preserved, longer common words are protected, and multiple matching targets are
rejected. The narrow consonant-skeleton exception preserves known name fixtures.
No claim of calibrated word confidence is made.

## Verification and measurements

Model-free Rust tests cover edit-distance breakdown, Unicode handling,
formatted tokens, exact Whisper number/contraction/spelling normalization,
keyword boundaries, repeated keyword insertions, buckets and percentiles. Pinned
configuration tests cover overrides, invalid values and unknown settings.
Python tests verify deterministic augmentation, achieved SNR, silence durations,
long-form checkpoint positions and golden/checkpoint scoring helpers.

Real-model smoke testing uses the existing public JFK clip, its repeated
longer clip and pure silence. These contain 88 reference words in total.
Aggregate summaries live in [eval-results](eval-results/README.md); audio,
per-case transcripts and private mismatch details remain outside committed docs.
This smoke set is too small and repetitive to estimate general accuracy or
choose a decoder/VAD/thread setting. Debug-build inference measurements are
smoke evidence, not release performance claims.

The three-case smoke evaluator reported **0 strict WER**, **0 Whisper WER**
and **8.65% formatted WER**. Formatting differences remain visible instead of
being hidden by normalization. A separate direct-worker before/after probe
found VAD-loaded PSS of 1,458,297 versus 956,040 KiB, and post-long-decode PSS
of 1,898,049 versus 905,188 KiB. These are single-run samples, not medians;
allocator trimming and model-type selection changed together, so effects cannot
be attributed individually. Peak RSS and retained PSS measure different things.

Stitched JFK fixtures of at least 60/120/180 seconds (390.38 seconds total)
report 0 strict/Whisper WER, 8.81% formatted WER and keyword precision/recall/F1
of 1.0 (93 true-positive occurrences for Americans/country). This establishes
word preservation for repeated public speech under those cuts, not general
long-form accuracy. Worker peak RSS was 1,107,388 KiB in the final debug-worker run.

The exact real-model invariance gate currently **fails** four comparisons:
JFK with an interior gap, and repeated JFK with trailing silence, a trailing
click, or an interior gap. All four differ only in case or punctuation; normalized
word sequences match exactly (word-LCS 1.0). The test remains strict and exits
nonzero. Short trailing silence, the 20 ms half-scale click, short leading
silence and pure silence pass. These results do not establish exact formatting
invariance across chunk/VAD boundaries.

Quiet-input A/B testing found 6/22 word errors at −40 dBFS with VAD enabled,
versus zero with VAD disabled or opt-in gain enabled. Original and −30 dBFS
versions had zero word errors in all three configurations. This supports the
opt-in control for this clip, not a general noisy-speech accuracy claim.

An earlier isolated nested-niri capture/paste smoke passed with a release build.
One measured sample after warm-up took 1,161 ms from capture end to paste,
with 990,050 KiB loaded process-tree PSS. This replay uses a PipeWire capture
shim and a controlled GTK input; it does not establish microphone latency or
a statistically significant speed improvement. The earlier debug sample took
809 ms, illustrating why single-run timings should not select defaults.
The isolated desktop suite passed settings, delivery, clipboard
restoration, raw-text recovery, cancellation, pending-open and shutdown checks.
Its fixture now waits for at least 350 ms of captured audio before expecting
delivery, satisfying the new 300 ms production minimum.
Release compilation also passed. CI builds GTK4 layer-shell from pinned commit
`69ad9a02a8c31ac2d3db6710f43b3d8434559e46`, because the selected Ubuntu 24.04
image lacks that GTK4 package. The source-build commands passed locally; the
hosted workflow has not been run.

## Public benchmark

The pinned public baseline scored 1,400 clips and 10,529.44 seconds of audio.
Unified reported 6.44% Whisper-normalized micro WER and 6.86% macro WER across
seven equally sampled sets. Strict WER was 8.28% and formatted WER was 21.87%.
Decode and cleanup took 406 ms at p50 and 973 ms at p95, with a worker peak RSS
of 1,202,944 KiB. These timings use one inference per case and include desktop
load on the test host. The baseline includes 69 clips below the 300 ms app
capture minimum. Published Sayso numbers use different sampling and short-clip
padding. The user selected public speech for this pass.

Full scores and asset fingerprints are in [public-unified.json](eval-results/public-unified.json).
Corpus counts, revision, and sampling are in [public-corpus.json](eval-results/public-corpus.json).

The 35-configuration sweep used the same 21 clips for each comparison. Six
threads reached 643 ms p95 versus 790 ms at four threads, with identical WER.
The arena-off CPU session lowered sampled peak process-tree PSS from 999,224
to 953,349 KiB in this run. These samples are too small to select production
thread or allocator settings.

No boost score improved keyword F1 beyond the greedy baseline of 97.44%.
At 3.0, boosting inserted seven keywords into ten control clips. The public
sample supports leaving boosting opt-in. Giving the decoder the reference
words at boost 1.5 increased raw errors from 15 to 29 across 120 reference
words. The [hotword sanity gate](eval-results/public-hotword-sanity.json) failed.

The full Unified run wrote digits in 43 clips and adjacent spoken number words
in 54 clips. These counts are a proxy for formatting opportunities. In the
sweep, opt-in number formatting lowered formatted WER from 25.77% to 25.21%
while increasing Whisper WER from 7.77% to 8.09%.

Low Memory and Unified each scored 7.77% WER on the 21 matched clips. Low
Memory reached 318,432 KiB peak worker RSS versus 998,100 KiB for Unified.
Its p95 was 329 ms versus 790 ms. Staged activation verified the archive
checksum and model settings and preserved prior preferences and backup.

On the full matched corpus, Low Memory scored 7.48% micro WER and 8.25%
macro WER. Unified scored 6.44% and 6.86% respectively. Low Memory's peak
worker RSS was 470,892 KiB, 61% below Unified's 1,202,944 KiB. Low Memory had
the largest WER increase on AMI, from 9.99% to 15.43%. The full Low Memory run
overlapped other evaluations, so use the small serial sweep for an exploratory
latency comparison. Unified remains the default.
[Full Low Memory results](eval-results/public-low-memory.json),
[matched sweep](eval-results/public-sweep.json),
[Low Memory sample](eval-results/public-low-memory-sample.json).

From seven public clips, 35 silence/noise variants scored 15.19% WER with
the baseline, 15.06% with capped gain, 15.68% with VAD off, 18.64% with
denoising, and 18.15% with VAD off and denoising on. Denoising also increased
p95 processing time from 1.35 to 3.39 seconds with VAD enabled. These results
support retaining the current denoiser default. On the same seven source clips,
28 speed and codec variants scored 10.03% WER overall.
[Noise and silence results](eval-results/public-augmented-baseline.json),
[media results](eval-results/public-media.json).

The 64-, 124-, and 186-second fixtures used 40 distinct LibriSpeech clean clips.
All 40 checkpoint phrases survived in order. Word-LCS exceeded 99.6% across
all three recordings. Worker high-water RSS stayed at 1,091,548 KiB after
the shortest fixture. This checks boundaries on individually decoded clean
clips. [Long-form scores](eval-results/public-long-boundary.json),
[checkpoint and memory results](eval-results/public-long-checkpoints.json).

Paced replay in isolated niri pasted the 11-second clip at 776 ms p50 and
783 ms p95 across three runs after warm-up. The 34.5-second replay completed
with 489–502 ms of post-capture decoding and cleanup. Its two stop-to-paste
samples took 7.70–8.33 seconds, with most of that time in delivery. Clipboard
tracing found reads returning the correct bytes after more than two seconds.
A three-second read deadline, matching writes, resolved the copy-only fallback
in the rerun. The focus and clipboard equality checks still gated paste.
The longer test includes tracing overhead and nested compositor stalls.
[Short replay](eval-results/desktop-paced-short.json),
[long replay](eval-results/desktop-paced-long.json).

Final local checks passed `make check`, `make check-python` (88 discovered,
three skipped), `make release`, the isolated desktop driver, and a staged
installation and rollback check. A short real PipeWire probe received its first
frame after 336 ms and discarded the audio. It did not exercise sustained
microphone capture under inference load.

## Follow-through against both plans

| Plan | Reference | Follow-through |
|---|---|---|
| Step 0, A8 | 9 | Prepared 1,400 public clips, 200 per set, with pinned revision and fixed seed. Added speed, MP3 and Opus transforms, per-set macro WER, confidence counts and a sweep runner. Personal recordings require a supplied corpus. |
| A1, A3, A4 | 1, 10, 11 | Conservative fuzzy matching and echo filter, plus opt-in confidence gating. Added a reference-hotword sanity gate and keyword/control sweeps. Keep boosting opt-in until it improves recall on representative jargon without hurting controls. |
| A2, L1, M1, M4 | 6, 12 | Bounded pieces release audio after decode. Cancellation discards queued pieces. Typed results preserve total input duration, including skipped silent chunks. Real microphone contention and allocator peak comparisons remain hardware measurements. |
| A5, A6, L3 | 2, 11 | Opt-in capped gain and one energy-gated retry. The harness compares gain, VAD, denoise, thread count, blank penalty, beam paths, piece length and model-local ORT options. Production defaults require larger paired comparisons. |
| A7 | 13 | Added opt-in number formatting using guarded `text-processing-rs` grammars. Preserve prose number words and protected spans. |
| A8 | 9 | Exact invariance remains available and reports punctuation differences. `--word-invariance` separately checks normalized word sequences while recording exact-format differences. |
| L2 | 15 | Tested the fallback and held-modifier behavior. Live modifier polling needs readable keyboard devices. |
| M2, M3 | 20 | Added shorter idle unload options and a checksum-pinned, opt-in Parakeet 110M model. The low-memory model uses 80 features and greedy decoding. Unified stays the default. |
| A9 | 10 | Parse word confidence, remap timestamps to source audio, and apply optional anomaly filtering. Missing native duration data never turns a pause into an anomaly penalty. Thresholds still need representative calibration. |
| — | 8, 19 | Opt-in private spool, crash recovery, failed recordings and history. Retry copies the result. Retain 20 WAVs per collection. Deletion is explicit, and recovery resumes safely after a crash during WAV promotion. |
| A9 | 14, 16 | Added opt-in spoken punctuation, bounded restarts and filler capitalization. Exact whole-utterance commands preserve literal phrases within ordinary speech. |
| — | 18 | Hold mode generates native press/release bindings and validates them before saving. Installed niri 26.04 rejects those bindings, so existing shortcuts stay active. Evdev support remains open. The test account has no readable evdev event devices for verification. |
| A9 | 17 | Learning from edits depends on reliable AT-SPI text access. The local accessibility bus advertised an address but refused connections. |
| M5, A9 | 5, 20–24 | Worker allocator trimming ships. Global allocator changes, alternative GPU/NPU engines, sherpa 2.x, LLM cleanup, personalized fine-tuning, batch decoding and per-stream hotwords remain experiments. The documents condition these on evidence or label them optional. |

See [eval-results/README.md](eval-results/README.md) for repeatable commands and measurement details.
