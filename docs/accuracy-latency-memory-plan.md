# Hype Flow: accuracy, latency and memory plan

Date: 2026-11-23. Branch: `migrate/rust-gtk4`.

This plan compares Hype Flow with 14 open-source speech projects and lists what
to build, in order, to improve accuracy, latency and memory use. Every claim
was checked against source code. Each effect is labelled by where its number
comes from:

- **Measured**: Hype Flow's own numbers, from `docs/migration-results.md` or
  from probes run while writing this plan.
- **Upstream**: a number published by another project, for another model or
  machine.
- **Estimate**: judgement, not yet measured.

## Contents

1. [Summary](#1-summary)
2. [Where Hype Flow stands](#2-where-hype-flow-stands)
3. [Reference projects](#3-reference-projects)
4. [Verified findings](#4-verified-findings)
5. [Step 0: the measurement harness](#5-step-0-the-measurement-harness)
6. [Accuracy work](#6-accuracy-work)
7. [Latency work](#7-latency-work)
8. [Memory work](#8-memory-work)
9. [Order of work](#9-order-of-work)
10. [Already done in Hype Flow](#10-already-done-in-hype-flow)
11. [Ruled out](#11-ruled-out)
12. [Open questions](#12-open-questions)
13. [Source index](#13-source-index)

---

## 1. Summary

- **Strengths.** Hype Flow has the strongest privacy and safety engineering of
  the projects reviewed:
  - the recognizer worker runs without network access;
  - model and runtime downloads are pinned by SHA-256;
  - speech segments are padded generously;
  - paste is terminal-aware, and the clipboard is restored only when it is
    safe to do so;
  - the evaluator reports WER both before and after cleanup, which no other
    project does.
- **Main weakness: accuracy is barely measured.** The only accuracy test is an
  88-word clip, scored with a custom normalizer. Sayso, NeMo, faster-whisper
  and whisper.cpp score on public datasets with a standard normalizer.
- **One real accuracy bug.** Fuzzy vocabulary correction rewrites ordinary
  words, for example "open it" becomes "OpenAI it". It is on by default. See
  [A1](#a1-fix-fuzzy-vocabulary-correction).
- **Biggest structural risk: long dictations.** A dictation of up to 300 s is
  decoded in one pass. Upstream measurements on related Parakeet models show
  large WER and memory penalties for long single-pass decodes. See
  [A2](#a2-decode-long-dictations-in-pieces-cut-at-pauses).
- **Biggest latency win: decode while the user talks.** After the user stops,
  only the last piece is left to decode. See
  [L1](#l1-decode-while-the-user-talks).
- **Memory.** Most memory is the model itself. Chunked decoding removes the
  spikes that long dictations cause. Idle unload already exists.

## 2. Where Hype Flow stands

### Pipeline

| Stage | Implementation | Location |
|---|---|---|
| Capture | `pw-record --raw --format s16 --rate 16000 --channels 1 --latency 50ms`. PipeWire resamples. | `src/audio.rs:166-180` |
| Recording cap | 300 s | `src/backend.rs:19` |
| Worker | sherpa-onnx **1.13.8** C API, in its own network namespace | `src/setup.rs:23-24`, `worker/src/main.rs` |
| Model | `parakeet-unified-en-int8`. Encoder 654 MB on disk. | `src/setup.rs:27-31` |
| Decoding | `greedy_search`. When boosting is on: `modified_beam_search`, `max_active_paths = 4`, `hotwords_score` | `worker/src/main.rs:204-214` |
| Threads | `min(cpus, 4)` | `src/config.rs:106` |
| Denoiser | GTCRN, optional, off by default. Runs before VAD. | `worker/src/main.rs:248-266, 300-306` |
| VAD | Silero: threshold 0.3, min_silence 0.45 s, min_speech 0.06 s, max_speech 30 s, window 512 | `worker/src/main.rs:222-232` |
| Segment padding | `PAD_BEFORE` 7200 samples (450 ms), `PAD_AFTER` 3200 (200 ms). Segments are joined end to end. | `worker/src/main.rs:20-21, 358-399` |
| Decode | All kept speech goes into **one** offline stream, decoded once | `worker/src/main.rs:325-342` |
| Cleanup | Fillers, corrections, blocked words, repetitions, fuzzy vocabulary (soundex + Levenshtein, tolerance 0.18) | `src/cleanup.rs` |
| Vocabulary boosting | Off by default. Score 1.5, allowed 0.5–3.0. Each term adds case variants; terms starting with `:` are rejected. | `src/config.rs:110-111, 181`, `src/vocabulary.rs:96-110` |
| Delivery | Clipboard, fixed 180 ms wait, focus check, paste key by app, restore after 800 ms only if unchanged | `src/output.rs:42, 213, 336-346` |
| Idle unload | Default 10 min; choices 0/5/10/15/30/60 | `src/config.rs:15, 124` |

### Baseline (measured; medians from `docs/migration-results.md`)

| Metric | Value |
|---|---:|
| Model-free process-tree PSS | 28 MB |
| Loaded process-tree PSS | 1.44 GB |
| Startup to ready (model loaded) | 4795 ms |
| Shortcut to first captured frame | 168 ms |
| End of capture to pasted text | 1171 ms |
| ├ decode + cleanup | 814 ms |
| └ delivery | 331 ms |
| Denoiser cost (earlier probe) | about +0.4 s |
| Accuracy on the owner's voice | not measured |
| Accuracy on public data | not measured |

### Evaluator today (`src/bin/evaluate.rs`)

- Reads a JSONL manifest of `{audio, text}` pairs.
- Normalizes text with NFKC, casefolding and removal of all punctuation.
- Reports per-case raw and cleaned errors and seconds, plus the overall WER.
- `--max-wer` sets the exit code.
- It loads the **live user config** (`Config::load()`), so results change
  whenever the user's settings change.
- It does not report:
  - insertions, deletions or substitutions separately;
  - a keyword metric;
  - case- or punctuation-aware WER;
  - latency percentiles or memory.
- There is no CI (no `.github/`).

## 3. Reference projects

All repositories were cloned under `/home/dj/tmp-whisper`.

| Project | Commit | What it is | Relevance |
|---|---|---|---|
| Sayso | `a7c4d85` | Go + Rust dictation app; sherpa-onnx **1.13.8** on Linux | Closest match to Hype Flow. Public benchmark harness. |
| Handy | `4807c21a` | Tauri dictation app, transcribe-cpp engines | Model catalog with English WER (FLEURS); filter for output that repeats the vocabulary; 363 Rust tests |
| OpenWhispr | `2617e0a` | Electron app; local sherpa-onnx/whisper.cpp plus cloud providers | Measured per-model hotword scores; repeat filter; learns corrections from user edits |
| Voxtype | `4d26c1f` | Rust Wayland dictation daemon | Pure-Rust ITN; DoubleMetaphone vocabulary matching; niri terminal detection |
| Sussurro | `0c1db47` | Go app, whisper.cpp + llama.cpp | Real-model invariance tests; bounded phonetic correction validator; LLM guards |
| Vocalinux | `7ad089e` | Python app (whisper.cpp, VOSK, Parakeet) | Before/after performance benchmarks; ~170 pytest files |
| OpenSuperWhisper | `c8e6fe7` | macOS Swift app | WER regression test; long-form fixtures with checkpoint phrases |
| NeMo / Speech | `c02239a` | NVIDIA NeMo Speech (both clones are the same repo) | Evaluator design, keyword F-score, boosting sanity test, fine-tuning |
| sherpa-onnx (main) | `3bf65c5` (v2.0.3) | Inference runtime | Model notes, streaming examples, 2.0.x decoder changes |
| sherpa-onnx v1.13.8 | `11afbd0` | Version pinned by Hype Flow | Used to confirm runtime behaviour |
| whisper.cpp | `5eaee0cc` | Inference + tests | Parakeet LibriSpeech eval, golden-transcript tests, Whisper normalizer |
| faster-whisper | `6cb639e` | Inference + benchmarks | WER/speed/memory benchmarks, hotword test |
| WhisperX | `8d8e43b` | Alignment | Low relevance: Parakeet already gives timestamps |
| Silero VAD | `066409f` | VAD model + tuning tools | Threshold grid search |
| llama.cpp | `44d50873b` | Local LLM runtime | Optional LLM cleanup with GBNF/JSON schema |

## 4. Verified findings

### 4.1 Bug: fuzzy correction rewrites ordinary words (measured)

The real Rust `cleanup::process` was run with default settings, putting each
vocabulary word in the prompt:

```
vocab ChargeBee | "charge back the card" -> "ChargeBee the card"
vocab OpenAI    | "open it"              -> "OpenAI it"
vocab Postgres  | "post grass"           -> "Postgres"
vocab Sinead    | "I signed the form"    -> unchanged
vocab Rust      | "I trust you"          -> unchanged
```

Cause, in `closest()` at `src/cleanup.rs:244-272`:

- A soundex match multiplies the distance ratio by 0.3 (`:265`), so ratios up
  to 0.6 pass the 0.18 tolerance.
- Common words and short words are not excluded.
- An ambiguous match is not rejected: the first best score wins.

The Python oracle (`src/dictation/cleanup.py:77-92`) behaves the same way.

### 4.2 Long single-pass decoding (upstream; unmeasured for Hype Flow's model)

- sherpa-onnx `scripts/nemo/parakeet-tdt-0.6b-ultra/README.md:106`: decoding a
  184.6 s clip in one pass gave **37.65% WER and +2335 MB RSS**. Splitting at
  silences into 15 s pieces gave **9.54% WER and +196 MB**.
- `scripts/nemo/parakeet-tdt-0.6b-v2/README.md:79`: one-pass decoding became
  unstable after about 69 s and cut off 57 of 141 words at 68.7 s.
- Hype Flow's model (`parakeet-unified-en-0.6b`) was trained with
  `max_duration: 40.0` (`scripts/nemo/parakeet-unified-en-0.6b/notes.md:94`).
  Its long-audio behaviour has not been measured. It is likely, but not
  proven, to degrade in the same way.

### 4.3 VAD behaviour in sherpa-onnx 1.13.8 (confirmed in source)

- **Max-speech switch.** Once the buffer holds more than `max_speech_duration`
  of audio, the threshold is raised to 0.90 and min_silence lowered to 0.1 s
  (`sherpa-onnx/csrc/voice-activity-detector.cc:51-53, 227-229`). The buffer
  is cleared at every detected pause, so this only fires after more than 30 s
  of speech with no pause of 0.45 s or longer. It then forces one cut, and
  Hype Flow's padding re-covers most of the gap. **Low risk.**
- **Built-in pre-roll** is `2 × window + min_speech` = 1984 samples (124 ms)
  (`:96-98`). Hype Flow adds 450 ms, about 574 ms in total. Fine.
- **Flush** ignores a final partial window of at most 31 ms (`Flush()`; fixed
  upstream in 2.0.2, #4147). `PAD_AFTER` covers it. Negligible.
- `neg_threshold` is not exposed in the 1.13.8 C API. It defaults to
  `threshold − 0.15` = 0.15.

### 4.4 Vocabulary boosting on Parakeet Unified (upstream)

- `openwhispr/src/utils/sherpaHotwords.js`: "Parakeet Unified ignored a bonus
  of 3 and collapsed at 8". OpenWhispr turns hotwords off for any model
  without a measured score, which includes Unified. It caps lists at 100 terms.
- Hype Flow allows 0.5–3.0 on Unified, and no score has been measured.

### 4.5 Model choice (upstream; confirmed)

Handy `src-tauri/src/catalog/catalog.json`, English, dataset `fleurs`:

| Model | WER |
|---|---:|
| parakeet-unified-en-0.6b | **3.99** |
| parakeet-tdt-0.6b-v2 | 4.11 |
| parakeet-ultra | 4.26 |
| parakeet-tdt-0.6b-v3 | 4.83 |
| parakeet-tdt_ctc-110m | 6.11 |

Sayso `docs/benchmark.md`, 1,400 clips across 7 Open ASR sets:

- parakeet-unified-en scores 6.29 mean WER on Core ML.
- parakeet-tdt-v2 scores **6.72 on sherpa-onnx CPU (Linux)**.

Hype Flow already ships the best English Parakeet. Switching models is not a
lever. Hype Flow's **int8** build has never been compared with fp32.

### 4.6 sherpa-onnx API facts (1.13.8 unless noted)

- NeMo transducers support only `greedy_search` and `modified_beam_search`
  (`offline-recognizer-transducer-nemo-impl.h:50-71`).
- `lm_config` is **not** used by the NeMo transducer path, so there is no LM
  fusion.
- ITN (`rule_fsts` / `rule_fars`) and the homophone replacer do run on the NeMo
  path (`:218-219`).
- `blank_penalty`, `hr` and `ys_log_probs` (per-token confidence) exist in the
  1.13.8 structs (`worker/src/bindings.rs:517, 519, 565`).
- `SherpaOnnxCreateOfflineStreamWithHotwords` (per-utterance hotwords) exists
  in 1.13.8 (`c-api.h:1333`).
- `length_reward` and the rewritten TDT beam search are **2.0.x only**.
- The 1.13.8 C API has no setting for the ONNX Runtime memory arena or memory
  mapping.

### 4.7 Corrections to earlier drafts of this comparison

| Earlier claim | Correct |
|---|---|
| Hype Flow ships v3-int8 and should switch to v2 | It ships unified-en-int8, the best of the lot |
| Long dictations lose quiet words after 30 s | Only after 30 s with no pause; one forced cut; low risk |
| Add speech padding | Already done (450/200 ms) |
| Add terminal paste key, PRIMARY, newer-copy-wins restore | Already done (`src/output.rs`) |
| Add hotword case variants | Already done (`src/vocabulary.rs:104-109`) |
| Handy has 6 test files | It has 363 Rust tests |
| OpenSuperWhisper has 1 test file | It has 22 test files, including a WER regression test |
| OpenWhispr is mostly cloud | It also runs local sherpa-onnx Parakeet and whisper.cpp |
| Voxtype's CI checks accuracy with real inference | It is a QEMU check that a baseline-CPU build does not crash (SIGILL) |

---

## 5. Step 0: the measurement harness

Build this first. Without it no change below can be ranked or verified.
Effort: 1–2 days.

### 5.1 Evaluator changes (`src/bin/evaluate.rs`)

1. **`--config <path>`.** Load a pinned config instead of `Config::load()`.
   Commit `docs/eval.toml` with the defaults.
2. **Two normalizers.**
   - `strict`: the current one.
   - `whisper`: a port of Whisper's `EnglishTextNormalizer`, from
     `whisper.cpp/tests/librispeech/normalizers/english.py` plus
     `english.json` (number words, British-to-American spellings,
     contractions, filler removal). This is the Open ASR Leaderboard standard,
     so results become comparable with published numbers.
3. **Error breakdown.** Backtrack the edit-distance matrix in `edits()` to
   count substitutions, insertions and deletions.
4. **Formatted WER.** A third mode that keeps case and punctuation and only
   splits punctuation from words. Source: NeMo
   `examples/asr/speech_to_text_eval.py:103-110`.
5. **Keyword metrics.** For each term in the vocabulary prompt, count true
   positives, false positives and false negatives against the reference.
   Report precision, recall and F1. Source: NeMo
   `scripts/asr_context_biasing/compute_key_words_fscore.py` and
   `nemo/collections/asr/parts/context_biasing/context_biasing_utils.py:152`.
6. **Length buckets.** WER per reference-audio length: <5 s, 5–20 s,
   20–60 s, >60 s.
7. **Performance.**
   - p50 and p95 of per-case seconds;
   - real-time factor (audio seconds ÷ processing seconds);
   - peak worker RSS, read from `/proc/<pid>/status` `VmHWM`;
   - for speed comparisons, take the minimum of N repeats (faster-whisper
     `benchmark/speed_benchmark.py:18-27`).
8. **Tags.** An optional `tags` array per manifest line, with a WER breakdown
   per tag.
9. **Output.** Keep the JSON lines and add a final summary object, so results
   from different configs can be compared in a small table.

### 5.2 Datasets

| Set | Contents | Source |
|---|---|---|
| Public | 200 clips × 7 Open ASR Leaderboard sets (AMI, Earnings22, GigaSpeech, LibriSpeech clean/other, SPGISpeech, VoxPopuli), fixed seed, resumable | `sayso/scripts/benchmark/prepare.py:26-37`, `score.py` |
| Personal | 100–200 of the owner's dictations, tagged `quiet`/`noisy`, `short`/`long`, `jargon`, `names`. **Kept outside git.** | new |
| Long-form | Public clips stitched into 60, 120 and 180 s recordings with 0.2–1 s gaps | sherpa Parakeet READMEs (4.2) |
| Augmented | Each clip with 0–5 s of silence at start and end, and noise at 0/5/10/15 dB SNR | NeMo `tools/asr_evaluator/conf/eval.yaml:24-38` |
| No-vocabulary control | Clips containing none of the vocabulary terms, to detect boosting and fuzzy false positives | new |

### 5.3 Done when

One command produces, for each configuration:
- WER under both normalizers, plus formatted WER;
- substitution, insertion and deletion counts;
- keyword precision, recall and F1;
- WER per length bucket and per tag;
- p50 and p95 latency, real-time factor and peak RSS.

Store each run's summary JSON under `docs/eval-results/` (summaries only,
never audio).

---

## 6. Accuracy work

### A1. Fix fuzzy vocabulary correction

- **Status:** verified bug (4.1). **Effort:** about 2 h. **Expected effect:**
  removes false rewrites of ordinary words.
- **Change `closest()` in `src/cleanup.rs:244`:**
  1. Skip candidates with 5 letters or fewer, and candidates that are common
     English words. Ship a ~5,000-word frequency list as a const. Source:
     Voxtype `src/text/vocabulary.rs:97-146`, which uses
     `term.letters.len() <= 5 || common_word(...)`.
  2. Require exactly **one** key under tolerance. If two or more keys
     qualify, do not replace. Source: Voxtype `vocabulary.rs:143`
     (`matches.len() == 1`).
  3. Make soundex a **gate**, not a multiplier: accept when the soundex codes
     match **and** `distance / longest ≤ 0.25`, or when there is no soundex
     match and `distance / longest ≤ tolerance`. Remove the `score *= 0.3`.
  4. Optional: add a consonant-skeleton distance of at most 1. Source:
     Sussurro `internal/llm/correction.go:465-487`.
  5. Optional: use DoubleMetaphone instead of soundex. Source: Voxtype uses
     `rphonetic::DoubleMetaphone`. It distinguishes more sounds than soundex.
- **Apply the same change to the Python oracle**
  (`src/dictation/cleanup.py:77-92`) if it is kept as an oracle.
- **Tests:**
  - Add the three false positives above to `tests/fixtures/cleanup.json`.
  - Keep the existing true positives passing: "Charge B" → ChargeBee,
    "open a i" → OpenAI, "Shunade" → Sinead, "R and D" → R&D.
- **Verify:** keyword precision on the no-vocabulary control set rises to
  about 1.0; keyword recall does not fall.

### A2. Decode long dictations in pieces cut at pauses

- **Status:** structural risk (4.2). **Effort:** 1–2 days. Also reduces memory
  (M1) and is the base for faster delivery (L1).
- **Change `speech_only()` and `process()` in `worker/src/main.rs`:**
  1. Keep VAD segments as a list instead of joining them.
  2. Group consecutive segments into **pieces of at most 15–20 s**.
  3. If one segment is longer than 20 s, cut it at the quietest 100–200 ms
     window within its last 4–5 s. Sources: Sayso
     `internal/localengine/audio.go:106-133` (quietest 200 ms in the last
     5 s); OpenWhispr `parakeetSegmentation.js` (quietest 100 ms within 4 s;
     a tail under 0.5 s joins the previous piece).
  4. Decode each piece in its own `SherpaOnnxCreateOfflineStream`. Join the
     texts with single spaces, avoiding a double space when a piece ends in
     punctuation.
  5. Keep the 450/200 ms padding per segment. Between segments inside one
     piece, insert 0.1 s of silence instead of joining them end to end.
     Source: whisper.cpp `src/whisper.cpp:6885-6886`; OpenSuperWhisper
     `speechOnlySamples`.
  6. Raise VAD `max_speech_duration` from 30 to 60, so cuts are made by step 3
     and not by sherpa's 0.90-threshold switch.
  7. When cut-silence (VAD) is off, still split audio longer than 20 s with
     the quietest-window rule.
- **Verify:** WER in the >20 s buckets and on the long-form set should be
  flat with length, not rising. No regression in <20 s buckets. Add
  checkpoint phrases at the boundaries (A8).

### A3. Measure vocabulary boosting, then keep or remove it

- **Status:** unmeasured on this model; upstream says it barely works (4.4).
  **Effort:** half a day.
- **Do:**
  1. Sweep `boost_score` ∈ {0.5, 1.0, 1.5, 2.0, 3.0}, plus boosting off, on:
     - jargon-tagged personal clips (keyword recall should rise);
     - the no-vocabulary control set (WER and keyword false positives must
       not rise).
  2. Add NeMo's sanity test as an env-gated test: put each clip's reference
     words in the vocabulary, and WER must fall. Source: NeMo
     `tests/functional_tests/asr_transcribe_boost_ground_truth.py`.
  3. Cap the list at 100 entries **after** case variants. Today each term
     becomes up to 3 entries (`src/vocabulary.rs:104-109`). Source:
     OpenWhispr `MAX_SHERPA_HOTWORDS = 100`.
- **Decide:**
  - If no score improves keyword F1 without hurting the control set, remove
    the boosting option for Unified and rely on the fixed fuzzy correction
    (A1). This also keeps greedy decoding, which is faster.
  - If one score wins, make it the default and narrow the allowed range.
- **Later (needs sherpa 2.0.x):** `length_reward` stops beam search dropping
  speech after pauses.

### A4. Filter output that repeats the vocabulary list

- **Effort:** about 1 h. Only needed while boosting exists.
- Discard the transcript, or the repeated span, when it contains 3 or more
  vocabulary terms in list order covering at least 75% of the words. Sources:
  Handy `src-tauri/src/managers/transcription.rs:1627-1650`; OpenWhispr
  `src/utils/dictionaryEchoFilter.js:15-28`, which also catches a word looped
  3 or more times.
- Drop output made only of punctuation (Handy `transcription.rs:1741-1746`).
- Place both filters at the start of `cleanup::process`.

### A5. Normalize gain for quiet input

- **Status:** upstream issue (sherpa-onnx #4024, noted in the v2, Ultra and
  Redux READMEs); not verified on Unified. **Effort:** about 2 h.
- **Measure first** with quiet-tagged clips and clips scaled to −30 and
  −40 dBFS.
- If WER rises on quiet input: in the worker, peak-normalize each piece to
  about −3 dBFS before `AcceptWaveformOffline`. Cap the gain at +20 dB so
  silence and noise are not boosted.

### A6. Tune VAD and the denoiser with data

- **Effort:** about 2 h of runs, no code.
- Grid:
  - cut-silence on/off × denoise on/off, on the clean and augmented sets;
  - VAD threshold {0.2, 0.3, 0.4, 0.5};
  - min_silence {0.3, 0.45, 0.6}.
- Method: Silero `tuning/utils.py:326-356` (grid search against labelled
  speech). Here the metric is WER plus the invariance tests (A8).
- Context:
  - OpenWhispr turned VAD off for dictation because it stripped speech
    (`whisperVadConfig.js:24-28`).
  - Silero's defaults are 0.5 / 250 ms / 100 ms / 30 ms pad
    (`silero-vad/src/silero_vad/utils_vad.py:283-293`).
  - Hype Flow is more permissive (0.3, 60 ms) and pads more.

### A7. Number formatting (inverse text normalization)

- **Effort:** about 1 day. Check first whether Unified already writes digits.
  Count spoken-form numbers in the public-set output.
- If needed, add the `text-processing-rs` crate, a pure-Rust port of NeMo
  text processing ("twenty five dollars" → "$25"). Source: Voxtype
  `Cargo.toml:103-106, 245-248`, `src/text/itn.rs:270-287`.
- Run it at the end of `cleanup::process`, behind a new `format_numbers`
  setting.
- Score with the formatted WER mode (5.1, item 4).

### A8. Golden tests, invariance tests and CI

- **Effort:** about 1 day.
- **Invariance tests** (env-gated, e.g. `HYPE_FLOW_MODEL_TEST=1`, running the
  real worker). Copy the cases from Sussurro
  `internal/asr/vad_integration_test.go:16-95`:
  1. speech + 8 s of trailing silence gives exactly the same text;
  2. speech + 5 s silence + a key-click transient gives the same text;
  3. pure silence gives an empty string;
  4. leading and interior silence do not drop words.
- **Golden transcripts:** word-LCS similarity ≥ 1.0 for short clips and ≥ 0.95
  for long ones. Source: whisper.cpp `tests/parakeet-verification.h:85-102`,
  `tests/CMakeLists.txt:185-223`.
- **Boundary checkpoints:** long fixtures with known phrases at the start,
  middle and end, and around the 20 s piece boundaries from A2. Source:
  OpenSuperWhisper `OpenSuperWhisperTests/Fixtures/README.md`.
- **Keyword-presence assertion**, as in faster-whisper
  `tests/test_transcribe.py:217-235`.
- **CI:** add `.github/workflows/ci.yml` running `make check` (fmt, clippy,
  tests without a model). Keep the model-gated tests and the eval run local,
  for example in a pre-push hook: `hype-flow-evaluate --config docs/eval.toml
  --max-wer <baseline + margin>`.

### A9. Later or optional

| Item | Effort | Note |
|---|---|---|
| Upgrade sherpa-onnx to 2.0.x | medium | TDT beam search rewrite, `length_reward`, VAD Flush fix. Needs new release SHAs in `src/setup.rs`. Only worth it if boosting survives A3. |
| Gate fuzzy correction on confidence | small | Only correct words whose tokens have low `ys_log_probs` (present in 1.13.8). Pass them from worker to app. |
| Learn corrections from user edits | large | OpenWhispr `openwhispr-mobile/src/lib/correctionLearner.ts` (Levenshtein/len < 0.65, ≥ 3 letters). Hard on Wayland: Hype Flow cannot read the text field after pasting. |
| LLM cleanup | large | llama.cpp with a JSON-schema list of edits (`grammars/README.md`, `tools/server/README.md:585-603`). Apply Sussurro's guards: keep ≥ 50% of the content, a minimum word count, cut at hallucination markers, 350-word chunks (`internal/llm/llama.go:80-96, 313-360`). Estimated +0.3–1 s and +0.5–1 GB on CPU. |
| Spoken punctuation ("new line", "comma") | small | Sayso `internal/enhance/text.go:247`; Voxtype stage order `src/text/mod.rs:303-377`. |
| Fine-tune on the owner's voice | research | NeMo adapters (`examples/asr/asr_adapters`, dim 32, ~1000 steps). Hours on CPU, then ONNX re-export; adapter export not verified. |

---

## 7. Latency work

Target: end of capture to pasted text well below the measured 1171 ms.

### L1. Decode while the user talks

- **Status:** biggest latency lever. **Effort:** 2–3 days. **Depends on:** A2.
- **Today:** decoding starts after capture stops; decode + cleanup takes
  814 ms (measured).
- **Design:**
  1. Stream audio to the worker during recording instead of sending one blob
     at the end. Extend the stdin protocol with a "chunk" message and an
     "end" message, or send one request per finished piece.
  2. In the worker, feed the VAD continuously. When a piece from A2 is
     complete (a pause after at least about 5 s of speech, or the 20 s cap),
     decode it immediately and keep its text.
  3. On stop: flush the VAD, decode only the last open piece, join all
     texts, then run cleanup in the app.
  4. If the remaining tail contains no speech, skip the final decode.
     Source: Sussurro `internal/pipeline/pipeline.go:490-505`.
  5. On cancel, drop the stored texts and audio straight away.
- **Patterns:**
  - sherpa-onnx
    `rust-api-examples/examples/parakeet_tdt_simulate_streaming_microphone.rs:159-283`
    (per-segment decode; it uses VAD 0.5 / 0.25 / 0.25 / max 5 s);
  - Sayso `internal/localengine/redecode.go:12-19` (re-decode every 600 ms,
    commit at the quietest point 7–2 s back).
- **Expected effect (estimate):** after stop, only the last piece is decoded.
  For a 30 s dictation that should take decode time from about 800 ms to
  about 200–300 ms. Dictations under 5 s gain little.
- **Verify:** p50/p95 from end of capture to paste, by length bucket. Run a
  CPU-contention check: decoding during recording must not cause capture
  overruns (watch for `pw-record` underruns).
- **Optional later:** a live preview with the streaming Parakeet Unified
  export (`scripts/nemo/parakeet-unified-en-0.6b/run-streaming.sh`;
  1120/560/240 ms chunks). It costs memory and some accuracy.

### L2. Replace the fixed 180 ms pre-paste sleep

- **Effort:** about 2 h. **Expected effect (estimate):** up to −150 ms of
  delivery time.
- `src/output.rs:343` always sleeps 180 ms so the compositor releases the
  shortcut modifiers before the paste keys are sent.
- Options, best first:
  1. If capture stopped on the shortcut's **release** event, the modifiers
     are already up: sleep 0–30 ms.
  2. If the input layer (uinput/ydotool) can read modifier state, poll for
     release, with a 50 ms minimum and a 1500 ms cap. Source: OpenWhispr
     `src/helpers/clipboard.js:22-34`.
  3. Otherwise, measure 80 ms and 120 ms against VS Code, a terminal and a
     browser, and keep the smallest that never drops a key.
- Also check the 100 ms wait after a refocus (`src/output.rs:132`); it only
  runs when focus moved.

### L3. Tune the thread count

- **Effort:** about 1 h of runs. Sweep `threads` ∈ {2, 3, 4, 6, 8} and record
  real-time factor and p95 on the eval set.
- Meteor Lake mixes performance and efficiency cores, so more threads can be
  slower.
- References: Sayso uses `min(NumCPU, 8)` (`sherpa.go:17`); OpenWhispr uses
  `min(4, 0.75 × CPUs)` (`parakeetWsServer.js:41-46`).

### L4. Keep the fast path fast

- Keep denoise off by default; it costs about 0.4 s. Turn it on only if A6
  shows a WER gain on noisy clips.
- Greedy decoding is faster than beam search. Settling A3 can remove beam
  search entirely.
- `keep_mic_ready` (instant capture start) and the preloaded model already
  exist. Document the trade-off: the mic stays open, and memory stays
  allocated.

---

## 8. Memory work

Baseline: about 1.44 GB process-tree PSS with the model loaded; 28 MB without
it (measured). The int8 encoder is 654 MB on disk. The rest is ONNX Runtime
working memory.

### M1. Chunked decoding caps peak memory

- Comes with A2. Encoder self-attention makes working memory grow faster than
  audio length.
- Upstream: one 184.6 s pass added +2335 MB over the loaded baseline; 15 s
  pieces added +196 MB (4.2).
- Hype Flow allows 300 s, so long dictations can spike far above 1.44 GB
  today.
- **Verify:** worker `VmHWM` per length bucket, before and after A2.

### M2. Make idle unload more flexible

- Already built: default 10 min. Measured drop from 1.50 GB to 31 MB after
  unload.
- Add a 1 or 2 min choice, and an "after each dictation" choice for low-RAM
  machines (`UNLOAD_CHOICES` in `src/config.rs:15`).
- Reload takes about 4.8 s. Start loading the worker as soon as recording
  starts. With L1 the load overlaps the user speaking.

### M3. Offer a low-memory model (opt-in)

- `parakeet-tdt_ctc-110m`: 6.11 English WER vs Unified's 3.99 (Handy catalog),
  so about 50% more errors. Much smaller encoder (estimate: about a fifth).
- Offer it only as a "low memory" choice in the model setting. It needs its
  own pinned SHA and download entry in `src/setup.rs`.

### M4. Avoid extra copies

- The worker already drops the input buffer after `AcceptWaveformOffline`
  (`worker/src/main.rs:337-340`). Keep that when adding streaming (L1):
  store pieces as decoded text, not audio, once decoded.
- Check that the app side does not keep a second full PCM copy after handing
  audio to the worker.

### M5. Research only

- GGUF / parakeet.cpp engines (Sayso's Linux "fluid-speech-mini" via Vulkan;
  Handy's transcribe-cpp) may load weights more cheaply than ONNX Runtime.
  There are no memory numbers for them.
- The ONNX Runtime arena and memory mapping cannot be configured through the
  sherpa-onnx 1.13.8 C API. Changing them needs a patched build.

---

## 9. Order of work

| # | Item | Effort | Accuracy | Latency | Memory | Proof |
|---|---|---|---|---|---|---|
| 0 | Measurement harness (5) | 1–2 d | enables all | enables all | enables all | one-command table |
| 1 | A1 fuzzy correction fix | 2 h | **high** (verified bug) | — | — | control-set keyword precision |
| 2 | A3 + A4 boosting decision, repeat filter | ½ d | medium | + if beam is dropped | — | keyword F1, control WER |
| 3 | A2 + M1 decode in pieces | 1–2 d | high on long dictation | — | **high** on long dictation | length-bucket WER, `VmHWM` |
| 4 | L1 decode while talking | 2–3 d | — | **high** | — | p50/p95 capture-to-paste |
| 5 | L2 pre-paste wait | 2 h | — | up to −150 ms | — | delivery p50 |
| 6 | L3 threads, A6 VAD/denoise grid | 2 h | medium | medium | low | eval table |
| 7 | A8 golden tests, invariance tests, CI | 1 d | prevents regressions | prevents regressions | — | CI green |
| 8 | A5 gain, A7 number formatting | 1–2 d | low–medium | — | — | quiet-tag WER, formatted WER |
| 9 | M2 faster unload, M3 low-memory model | ½ d | — | — | high for low-RAM users | PSS |
| 10 | A9 options (sherpa 2.0.x, LLM, learning from edits, fine-tuning) | large | situational | LLM adds latency | LLM adds memory | eval table |

---

## 10. Already done in Hype Flow

Do not redo these.

- Speech padding of 450 ms before and 200 ms after each VAD segment, on top of
  sherpa's 124 ms built-in pre-roll.
- Terminal-aware paste key using `niri msg --json focused-window`.
- PRIMARY selection writes.
- Clipboard restore only when the transcript is still on the clipboard.
- Hotword case variants; `:` tokens rejected.
- Soundex + Levenshtein fuzzy matching, the same approach as Handy, with the
  same 0.18 tolerance. It needs the A1 guards.
- 16 kHz mono capture through PipeWire, with no app-side resampler.
- SHA-256 pinned model and runtime downloads.
- Worker without network access.
- Idle model unload.
- Keep-mic-ready.
- Denoise off by default.
- Raw-vs-cleaned WER in the evaluator.

## 11. Ruled out

| Idea | Why |
|---|---|
| Switch to parakeet-tdt v2 or v3 | Unified is already the best English Parakeet (4.5) |
| LM shallow fusion | sherpa-onnx's NeMo transducer path ignores `lm_config` (4.6) |
| Punctuation model | Parakeet already outputs punctuation and case |
| sherpa homophone replacer | Uses a Chinese pinyin lexicon; no use for English |
| sherpa's shipped ITN FSTs | Only a Chinese FST ships (agent-reported); use `text-processing-rs` instead |
| WhisperX alignment | Parakeet TDT already gives token timestamps |
| Whisper decoding thresholds (temperature fallback, compression ratio, no-speech) | Whisper-specific; not applicable to a transducer |

## 12. Open questions

- How Unified handles long audio. Measure with the long-form set in step 0.
- The WER cost of int8 vs fp32 for Unified.
- Whether the quiet-input frontend issue (#4024) affects Unified.
- Whether Unified already writes numbers as digits.
- Whether any boost score helps Unified.
- llama.cpp cleanup latency on this CPU (`llama-bench` with a 0.5–1.5B Q4
  model).
- Whether NeMo adapters survive ONNX export.
- Sayso's dictation path appears not to use VAD; this was inferred from a
  search, not confirmed.

## 13. Source index

Paths are relative to `/home/dj/tmp-whisper/<project>/` unless they start with
`src/`, `worker/`, `tests/` or `docs/`, which are Hype Flow paths.

### Accuracy harness and metrics
- `sayso/docs/benchmark.md`
- `sayso/scripts/benchmark/prepare.py`, `score.py`
- `whisper.cpp/tests/librispeech/normalizers/english.py`, `english.json`
- `whisper.cpp/tests/librispeech-parakeet/eval.mk`, `eval.py`
- `whisper.cpp/tests/parakeet-verification.h`, `tests/CMakeLists.txt:185-223`
- `faster-whisper/benchmark/wer_benchmark.py`, `speed_benchmark.py`,
  `memory_benchmark.py`
- `NeMo/examples/asr/speech_to_text_eval.py`
- `NeMo/tools/asr_evaluator/conf/eval.yaml`
- `NeMo/scripts/asr_context_biasing/compute_key_words_fscore.py`
- `NeMo/tests/functional_tests/asr_transcribe_boost_ground_truth.py`
- `sussurro/internal/asr/vad_integration_test.go`
- `OpenSuperWhisper/OpenSuperWhisperTests/WhisperTurboRegressionTests.swift`,
  `Fixtures/README.md`

### Vocabulary and cleanup
- `voxtype/src/text/vocabulary.rs:97-146`, `src/text/itn.rs`,
  `src/text/mod.rs:303-377`
- `sussurro/internal/llm/correction.go:309-487`, `internal/llm/llama.go`
- `Handy/src-tauri/src/audio_toolkit/text.rs`,
  `src-tauri/src/managers/transcription.rs:1627-1650, 1741-1746`
- `openwhispr/src/utils/sherpaHotwords.js`,
  `src/utils/dictionaryEchoFilter.js`,
  `openwhispr-mobile/src/lib/correctionLearner.ts`
- `sayso/internal/enhance/text.go`

### Audio, VAD and chunking
- `sherpa-onnx-1.13.8/sherpa-onnx/csrc/voice-activity-detector.cc`
- `sherpa-onnx/scripts/nemo/parakeet-tdt-0.6b-ultra/README.md`,
  `parakeet-tdt-0.6b-v2/README.md`, `parakeet-unified-en-0.6b/notes.md`
- `sherpa-onnx/rust-api-examples/examples/parakeet_tdt_simulate_streaming_microphone.rs`
- `sayso/internal/localengine/audio.go`, `vad.go`, `redecode.go`
- `openwhispr/src/helpers/parakeetSegmentation.js`, `whisperVadConfig.js`
- `Handy/src-tauri/src/audio_toolkit/chunking.rs`, `vad/mod.rs`
- `silero-vad/src/silero_vad/utils_vad.py`, `tuning/utils.py`
- `whisper.cpp/src/whisper.cpp:6873-6886`

### Model data
- `Handy/src-tauri/src/catalog/catalog.json` (`results.*.accuracy`, dataset
  `fleurs`)

### Delivery
- `openwhispr/src/helpers/clipboard.js:22-34`
- `voxtype/src/output/active_window.rs`, `src/output/clipboard_restore.rs`

### Optional LLM cleanup
- `llama.cpp/grammars/README.md`, `tools/server/README.md:585-603`,
  `docs/speculative.md:181-186`

## Implementation follow-through

Current code, measurements, deviations and remaining evidence gates are tracked
in [implementation-status.md](implementation-status.md). Evaluation commands and
summary formats are documented in [eval-results/README.md](eval-results/README.md).

### Remaining work

- **A2, A8.** Improve recognition across silence and cut boundaries with the
  default CTC model. Two interior-silence comparisons changed words, and five
  other comparisons changed only case or punctuation. CTC preserved 36 of 40
  checkpoint phrases in order across the three long fixtures.
- **Step 0, A1, A3, A5–A7, A9, L3.** Personal and jargon accuracy are still
  open after the public-only pass. Larger paired runs on the default model
  are needed to tune quiet-input gain, VAD, denoising, and thread count.
  Calibrate number formatting and confidence controls where the decoder
  supplies the required metadata. Unified boosting remains experimental.
- **L1, L2, M1.** Measure live stop-to-paste p50/p95 by recording length and
  application. Check PipeWire for overruns during eager decode. Compare
  worker peak RSS by length against a single-pass baseline on matched audio.
- **A9, M5.** Edit learning, local LLM cleanup, personal fine-tuning, a
  live streaming preview, alternative inference backends, and a sherpa
  runtime upgrade remain optional. A/B tests should establish gains before
  changing allocator or runtime defaults.
