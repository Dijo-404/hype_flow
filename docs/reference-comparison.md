# Hype Flow compared with open-source dictation and ASR projects

Date: 2026-11-23. Branch: `migrate/rust-gtk4` (commit `9f647ec`).
References: the clones in `/home/dj/tmp-whisper`.

This report compares Hype Flow with sixteen reference projects. It lists what
Hype Flow already matches, the defects found while comparing, and a ranked list
of features and techniques worth adopting. Every recommendation is offline and
privacy-preserving; cloud features were excluded.

## Contents

1. [Method and evidence levels](#1-method-and-evidence-levels)
2. [Hype Flow baseline](#2-hype-flow-baseline)
3. [Summary](#3-summary)
4. [Defects and gaps found in Hype Flow](#4-defects-and-gaps-found-in-hype-flow)
5. [Where Hype Flow already matches or leads](#5-where-hype-flow-already-matches-or-leads)
6. [Ranked recommendations](#6-ranked-recommendations)
7. [Not recommended](#7-not-recommended)
8. [Per-project findings](#8-per-project-findings)
9. [Mapping to the optimization checklist](#9-mapping-to-the-optimization-checklist)
10. [Measurement plan](#10-measurement-plan)
11. [Open questions](#11-open-questions)

---

## 1. Method and evidence levels

The Hype Flow core (`src/*.rs`, `worker/src/main.rs`, `worker/src/bindings.rs`)
was read first and summarized as a baseline. The reference repositories were
then surveyed file by file in six parallel groups:

| Group | Repositories                                                      |
| ----- | ----------------------------------------------------------------- |
| A     | Handy, OpenSuperWhisper                                           |
| B     | OpenWhispr                                                        |
| C     | Voxtype, Sussurro                                                 |
| D     | Vocalinux, Sayso                                                  |
| E     | sherpa-onnx 1.13.8 (pinned), sherpa-onnx main (2.0.3), silero-vad |
| F     | whisper.cpp, faster-whisper, whisperX, NeMo, Speech, llama.cpp    |

The key claims behind the top recommendations were then checked directly
against the source files.

Evidence levels used in this report:

- **Verified**: confirmed by reading the cited code during the final check.
- **Read**: reported by a survey agent that read the code; not re-checked.
- **Estimate**: an inference about performance or behaviour. Nothing in this
  report was run or benchmarked.

Coverage limits:

- **Voxtype** has about 212,000 lines of Rust. About 30 focus files were read
  fully, and the rest were searched.
- **OpenWhispr** has 2,880 first-party files. The dictation, local ASR,
  dictionary, paste and lifecycle files were read. Mobile, cloud, account, UI
  components and most tests were skipped.
- **Sayso** translations, the macOS/Windows platform code, the cloud providers
  and the TypeScript SDK were skipped.
- **sherpa-onnx, NeMo, whisper.cpp and llama.cpp** were read only where
  relevant to Hype Flow's pipeline.
- **`Speech`** is the same repository as `NeMo`: same HEAD `c02239a`, and
  `diff -rq` is empty. The project was renamed from NVIDIA/NeMo to
  NVIDIA-NeMo/Speech.
- **The REA reverse-engineering toolkit** was not used. All targets are
  complete source repositories, and REA's own guidance says to use ordinary
  repository tools for those.

---

## 2. Hype Flow baseline before the accuracy and memory work

| Area       | Current implementation                                                                                                                                                                                                              |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Capture    | `pw-record --raw` s16, 16 kHz mono (`src/audio.rs`). 1 s pre-roll ring, optional keep-mic-ready, 5 s first-audio timeout, level meter.                                                                                              |
| Recognizer | Separate `hype-flow-recognizer` process (`worker/src/main.rs`). It loads the sherpa-onnx 1.13.8 C API via `libloading` and runs inside `bwrap --unshare-net --unshare-pid`.                                                         |
| Model      | `parakeet-unified-en-int8`: an offline transducer. Files: encoder 623.7 MB, decoder 6.9 MB, joiner 1.7 MB, `bpe.vocab`, `tokens.txt`. CPU only; threads = `min(cpus, 4)`.                                                           |
| Decoding   | `greedy_search` by default. With vocabulary boosting on: `modified_beam_search`, `max_active_paths = 4`, hotwords file, score 0.5–3 (default 1.5).                                                                                  |
| VAD        | Optional Silero, run on the whole recording after capture: threshold 0.3, min silence 0.45 s, min speech 0.06 s, max speech 30 s. Padding 450 ms before and 200 ms after. If no speech is found, all audio is decoded.              |
| Denoise    | Optional GTCRN offline denoiser before VAD.                                                                                                                                                                                         |
| Long audio | Splits at energy-based pauses into pieces of at most 15 s (`src/segments.rs`). Pieces are decoded one after another (`src/backend.rs`). Hard cap 300 s.                                                                             |
| Lifecycle  | Model loads on first use. Idle unload after 0/5/10/15/30/60 minutes. Cancellation by epoch. Loaded process-tree PSS is about 1.44 GB; model-free about 28 MB.                                                                       |
| Cleanup    | Filler removal, user corrections (`a -> b`), optional repetition removal (`src/cleanup.rs`). Fuzzy vocabulary matching uses Soundex and Levenshtein with tolerance 0.18. Case is preserved.                                         |
| Vocabulary | Hotwords are converted to SentencePiece BPE. Words beginning with `:` are dropped (`src/vocabulary.rs:100`).                                                                                                                        |
| Output     | `wl-copy` plus `wtype` Ctrl+V (Shift+Insert in terminals). The clipboard is restored after 800 ms, guarded by a generation counter. Also: niri focus check, PRIMARY selection, optional trailing space and Enter (`src/output.rs`). |
| Control    | GApplication D-Bus actions `toggle` and `cancel`, bound in niri (`docs/niri.kdl`). Toggle only, no push-to-talk.                                                                                                                    |
| UI         | GTK4 layer-shell overlay and notch, and a settings window. A "Copy original" action recovers the text before cleanup.                                                                                                               |
| Evaluation | The Rust and Python word-level evaluators used an 88-word smoke corpus.                                                                                                                                                             |

Missing at this checkpoint:

- Streaming or partial results.
- Number formatting (inverse text normalization, ITN).
- LLM cleanup.
- Use of token confidence or timestamps.
- Hallucination filtering.
- Push-to-talk.
- History or retry of failed recordings.
- Runtime profiles.
- GPU or NPU acceleration.

---

## 3. Summary

- **Hype Flow is already level with or ahead of the reference apps** on
  clipboard safety, worker isolation, IPC, idle unload and vocabulary boosting.
  Sayso and Vocalinux do not apply their dictionary to sherpa models at all.
- **The largest cheap wins need no new model.** sherpa-onnx already returns
  per-token log-probabilities and timestamps, and already supports a blank
  penalty and an explicit model type. Hype Flow uses none of them.
- **Two verified defects should be fixed first:**
  - The fuzzy vocabulary matcher rewrites common English words. It is on by
    default.
  - When VAD finds no speech, the whole recording is decoded. Every reference
    project returns nothing in that case.
- **The best latency improvement is decoding while recording.** Each closed
  segment goes to the already-loaded offline model during speech, so only the
  last segment remains after stop. Handy, Voxtype, Vocalinux and Sayso all do
  some form of this. True streaming Parakeet is not worth it on this CPU yet.
- **Evaluation should come first.** Several decisions depend on measurement
  that the 88-word smoke corpus cannot support: hotword score, blank penalty,
  chunk length, thread count and VAD on/off.

---

## 4. Defects and gaps found in Hype Flow

### 4.1 Fuzzy vocabulary rewrites common words (Verified by code trace)

`src/cleanup.rs:245-273` (`closest`) scores a candidate as
`edit_distance / longest_length` and multiplies the score by 0.3 when the
Soundex codes match. It accepts the best score below `fuzzy_tolerance`, which
defaults to 0.18.

Example with `Rust` in the vocabulary and the spoken word `rest`:

- Edit distance is 1 and the longest length is 4, so the score is 0.25.
- Both words have Soundex code `R230`, so the score becomes 0.25 × 0.3 = 0.075.
- 0.075 < 0.18, so `rest` is replaced with `Rust`.

`fuzzy_vocabulary` defaults to `true` (`src/config.rs:117`). Any short
vocabulary term with common sound-alike English words is affected. This was
traced by hand, not run.

Voxtype uses a stricter gate (`voxtype/src/text/vocabulary.rs:116-145,174-200`):

- Terms of 5 letters or fewer must match exactly.
- 6-letter terms allow edit distance 1; longer terms allow `len/4`.
- The DoubleMetaphone code must match. It is a requirement, not a discount.
- A correction is accepted only when exactly one vocabulary term matches.
- Tokens touching `@`, `/` or `\` are skipped, as are non-ASCII tokens.
- A common-word blocklist is applied.

### 4.2 "No speech" still decodes everything (Verified)

`worker/src/main.rs:311-321`: when Silero returns no segments, the worker
writes `vad heard no speech; decoding all … s` and decodes the whole buffer.

- The only app-side silence check is for all-zero PCM (`src/backend.rs:313`).
- Faster-whisper, whisper.cpp, OpenSuperWhisper and Sussurro all return empty
  output when VAD finds nothing.
- OpenWhispr and Voxtype add an energy gate so that very soft speech is not
  lost.

### 4.3 Stop cue can be captured (Verified ordering; leak not measured)

`src/app.rs:290-309`: `feedback::play_cue("stop", …)` runs on the main thread
before `recorder.stop()` runs in the spawned worker thread. With speakers
rather than headphones, the cue can be recorded into the tail of the dictation.
Vocalinux guards this with `stop_sound_guard_ms = 200`.

### 4.4 Vocabulary boosting may not help this model (Verified source; not reproduced)

`openwhispr/src/utils/sherpaHotwords.js:1-12` records a per-model measurement:

> "Parakeet Unified ignored a bonus of 3 and collapsed at 8"

OpenWhispr therefore disables hotwords for this model. It allows score 2 only
for `orukeet-v0.1.0` and caps the list at 100 words.

Hype Flow runs the same model with score 1.5 and switches to
`modified_beam_search` (4 active paths), which costs latency. Whether boosting
improves recall here is untested.

### 4.5 Model type is probed at startup (Verified)

`sherpa-onnx-1.13.8/sherpa-onnx/csrc/offline-recognizer-impl.cc:295-300`
returns the NeMo transducer implementation directly when
`model_config.model_type == "nemo_transducer"`. Otherwise
(`offline-recognizer-impl.cc:320-356`, Read) sherpa opens a throwaway ONNX
Runtime session on the 623.7 MB encoder just to read its metadata.

Hype Flow leaves `model_type` empty.

### 4.6 Token confidence and timestamps are discarded (Verified)

The NeMo greedy decoders in sherpa fill `tokens`, `timestamps` and
`ys_log_probs` (`offline-transducer-greedy-search-nemo-decoder.cc:60-90` and
`:141-171`). The worker forwards the full result JSON. `src/backend.rs:335`
reads only `"text"`.

### 4.7 Freed memory is never returned to the OS (Verified)

Handy `src-tauri/src/memory.rs` calls `mallopt(M_MMAP_THRESHOLD, 128 * 1024)`
at startup and `malloc_trim(0)` after each transcription, in both the app and
its worker. Hype Flow calls neither.

Handy's comments report retained memory falling from about 15 MB to 0.5 MB per
2-minute dictation (Read).

### 4.8 Smaller gaps

| Gap                                               | Evidence                                                | Reference                                                                                 |
| ------------------------------------------------- | ------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| No minimum recording length                       | none found in `src/`                                    | Sayso `MinDurationMs: 300`; Sussurro 300 ms; OpenSuperWhisper 1.0 s                       |
| `<unk>` and punctuation-only output not filtered  | no `<unk>` handling in `src/` or `worker/src`           | Vocalinux `_filter_non_speech`; Handy `transcription.rs:1745-1750`                        |
| VAD segments are concatenated with no gap         | `worker/src/main.rs:380-394`                            | whisper.cpp inserts 0.1 s of zeros between segments                                       |
| Fixed 180 ms sleep before paste                   | `src/output.rs:343`                                     | Voxtype `modifier_guard.rs`; OpenWhispr `MODIFIER_RELEASE_WAIT_MS = 1500` via `EVIOCGKEY` |
| VAD buffer sized for 300 s                        | `SherpaOnnxCreateVoiceActivityDetector(&config, 300.0)` | `CircularBuffer` zero-fills its capacity, about 19 MB; about 40 s is enough               |
| `remove_repetitions` collapses legitimate doubles | `src/cleanup.rs:340`                                    | Voxtype `LEGITIMATE_DOUBLES = ["had","that","very","no","yes","ha","so"]`                 |
| No warm-up decode                                 | the worker reports ready right after `Decoder::new`     | OpenWhispr decodes 1 s of silence first (`parakeetWsServer.js:416-428`)                   |
| Failed recordings are not kept                    | only "Copy original" for text                           | Voxtype `failed_recording.rs` keeps 20 WAVs; OpenWhispr crash spool                       |

---

## 5. Where Hype Flow already matches or leads

| Area                        | Hype Flow                                                                                                                             | Reference comparison                                                                               |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Fuzzy vocabulary algorithm  | Near line-for-line port of Handy's `apply_custom_words`: same 0.18 tolerance, 0.25 length gate, Soundex × 0.3, 3-grams, `&` expansion | Equal to Handy. Voxtype is stricter (see 4.1)                                                      |
| Clipboard restore           | Generation counter plus a check that the clipboard still holds the transcript; typed restore; `--sensitive`                           | Equal to Voxtype's "newer copy wins" and OpenSuperWhisper's `changeCount`                          |
| Paste key per app           | Shift+Insert for terminals, keyed by niri `app_id`                                                                                    | Equal to Voxtype `paste.rs`. The OpenWhispr terminal list (30 classes) is longer and worth syncing |
| Worker isolation            | Separate process, `bwrap --unshare-net --unshare-pid`                                                                                 | Handy and Voxtype also isolate workers; none deny network as strictly                              |
| Control IPC                 | GApplication D-Bus actions from niri binds                                                                                            | Cleaner than Voxtype's SIGUSR plus override files and Sussurro's socket                            |
| Vocabulary on sherpa models | Hotwords plus beam search                                                                                                             | Sayso and Vocalinux ignore the dictionary on sherpa models                                         |
| Punctuation and case        | Parakeet emits `, . ? !` and capitals (106 capitalised tokens)                                                                        | No punctuation model needed; Voxtype uses one only for CTC engines                                 |
| Noise suppression           | GTCRN                                                                                                                                 | Same as Voxtype `enhance.rs`                                                                       |
| Idle unload                 | Configurable minutes                                                                                                                  | Equal to Handy and Vocalinux keepalive                                                             |
| Hotword sanitising          | Drops words starting with `:`                                                                                                         | Equal to OpenWhispr. The 100-word cap and case-insensitive dedupe are not yet present              |
| Hook                        | `hook.rs`, 30 s timeout                                                                                                               | Equal to the Vocalinux post-processor script                                                       |

---

## 6. Ranked recommendations

Effort: S is under a day, M is a few days, L is a week or more.

### Tier 1: small changes, high value

**1. Tighten the fuzzy vocabulary gate.** Effort S. Fixes 4.1.

- Change `src/cleanup.rs::closest`:
  - Exact match for keys of 5 letters or fewer.
  - Maximum edit distance 1 for 6 letters, `len/4` above that.
  - Soundex (or DoubleMetaphone via `rphonetic = "4.0"`, as Voxtype uses) must
    be equal.
  - Reject ties between two different terms.
  - Skip words containing `@ / \`.
- Add regression tests: `rest` stays `rest` with `Rust` in the vocabulary, plus
  a sentence of common words that must remain unchanged.
- Source: Voxtype `src/text/vocabulary.rs:116-200`.

**2. Silence and junk guards.** Effort S. Fixes 4.2 and part of 4.8.

- Worker: when VAD returns no segments, return empty output instead of decoding
  everything.
- To protect soft speech, first apply an energy check in the app:
  - OpenWhispr: 100 ms windows; silence if peak RMS < 0.002; a speech window
    needs RMS ≥ 0.003 and peak ≥ 0.02 (`localSpeechGate.js`).
  - Or Voxtype's adaptive gate: floor = 10th-percentile frame RMS × 3,
    threshold clamped to `[cfg/10, cfg]`, 20 ms frames, at least 3 consecutive
    loud frames, minimum 100 ms speech (`vad/energy.rs:56-91`).
- App: discard recordings under 300 ms (9,600 bytes of s16) and show "Too
  short".
- After decode: strip `<unk>`; return empty when the result has no letter or
  digit, or only bracket tags.
- Retry once when audible audio decodes to an empty string (OpenWhispr
  `parakeetServer.js:236-277`).

**3. Set the model type.** Effort S. Fixes 4.5.

- In `Decoder::new`: `config.model_config.model_type = c"nemo_transducer".as_ptr();`
- Re-measure load time and peak RSS.

**4. Warm-up decode.** Effort S.

- After the recognizer is created, decode 16,000 zero samples before reporting
  ready. This moves first-decode allocation cost out of the first dictation.

**5. Allocator trimming.** Effort S. Fixes 4.7.

- Call `libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024)` at the start of both
  `main` functions.
- Call `libc::malloc_trim(0)` after each transcription in the worker and after
  delivery in the app.

**6. VAD housekeeping.** Effort S.

- Pass 40.0 instead of 300.0 as the VAD buffer size. Estimate: about 16 MB
  less.
- Insert 1,600 zero samples (0.1 s) between non-contiguous VAD segments in
  `speech_only` (whisper.cpp `src/whisper.cpp:6884-6930`).

**7. Close the microphone before the stop cue.** Effort S. Fixes 4.3.

- Take the recorder's PCM before playing `stop`, or trim about 200 ms from the
  end.
- OpenSuperWhisper's opposite technique, a 0.25 s stop tail, avoids clipping
  the last word. Choose one after measuring.

**8. Keep failed recordings and allow retry.** Effort S–M.

- On worker error or timeout, write a WAV (`backend::wav_bytes` already exists)
  to `$XDG_STATE_HOME/hype-flow/failed/`, with the directory at 0700 and files
  at 0600. Keep the last 20.
- Add a "Retry last" action that sends it back through `backend.transcribe`.
- Source: Voxtype `src/failed_recording.rs`.

### Tier 2: medium effort

**9. Evaluation harness.** Effort M. Prerequisite for 10, 11, 12 and 13.

- **Corpus:** 30–60 minutes of the owner's own dictation, including the
  vocabulary terms. Make offline copies with NeMo-style perturbation
  (`nemo/collections/asr/parts/preprocessing/perturb.py:457-476`):
  - noise at SNR 10–50 dB
  - speed 0.9–1.1
  - MP3/Opus transcode
- **Scoring:** WER after removing case and punctuation, as NeMo does with
  `ignore_capitalization` and `ignore_punctuation`. Alternatively use Sayso's
  method: Whisper `EnglishTextNormalizer` plus `jiwer`, WER per set then the
  mean, and skip timing the first call after load (`sayso/docs/benchmark.md`,
  `scripts/benchmark/score.py`).
- **Keyword metric:** NeMo `compute_fscore`
  (`context_biasing_utils.py:152-230`), giving precision, recall and F1 for
  vocabulary terms. Also count false insertions on speech without vocabulary
  words.
- **Regression fixtures:**
  - Sussurro: speech followed by 8 s of silence equals the speech alone; a
    click of amplitude 0.5 over 320 samples is ignored; 8 s of silence gives
    `""`.
  - OpenSuperWhisper: TTS audio of 90 s or more with checkpoint phrases near the
    15 s cuts, asserted in order and without duplication.

**10. Use token confidence.** Effort S–M. Uses 4.6.

- Parse `tokens`, `timestamps` and `ys_log_probs` in `backend.rs`. Group tokens
  into words at `▁`.
- Score each word by its minimum log-prob, as NeMo does with
  `aggregation="min"`.
- Run fuzzy vocabulary correction only on words below a confidence cutoff, so
  confident words are never rewritten.
- Add a hallucination check adapted from faster-whisper
  (`transcribe.py:1278-1296`). That check adds 1 per word with probability
  below 0.15 and penalises durations under 0.133 s or over 2.0 s. It flags an
  anomaly when the score is at least 3 over the first 8 words. Here, map
  probability to `exp(log_prob)` and duration to the timestamp deltas.
- Calibrate the cutoffs with the harness. A mean log-prob of about −1.0 is a
  starting guess.

**11. Decoder and runtime sweep.** Effort S each, after item 9.

| Setting                | Values to test                                                                                                                                   | Notes                                                                                                                                          |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `blank_penalty`        | 0, 0.5, 1.0, 2.0                                                                                                                                 | Already in `bindings.rs:517`. Applied by both the greedy and beam NeMo decoders when > 0. Lowers deletions and may raise insertions            |
| Hotwords               | off (greedy) / 1.5 / 3                                                                                                                           | If there is no keyword-F1 gain, default to greedy and rely on the fixed fuzzy layer                                                            |
| Per-term hotword score | `term :2.5` per line                                                                                                                             | sherpa `utils.cc:52-55`                                                                                                                        |
| Hotword cap            | 100 terms, case-insensitive dedupe                                                                                                               | OpenWhispr `MAX_SHERPA_HOTWORDS`                                                                                                               |
| `max_active_paths`     | 4 vs 8                                                                                                                                           | Beam only                                                                                                                                      |
| Threads                | 4, 6, 8                                                                                                                                          | Core Ultra 9 185H P-core/E-core placement                                                                                                      |
| ORT session options    | `provider = "cpu:<model>/session.cfg"` with `EnableCpuMemArena=0`, `EnableMemPattern=0`, optionally `SessionConfig.session.disable_prepacking=1` | Parsed by sherpa `session.cc:85-226`. Test each line separately for PSS and latency. The file must be inside the bwrap-visible model directory |
| Max piece length       | 15 s vs 30 s vs 60 s                                                                                                                             | Sayso uses 60 s for Parakeet TDT; Handy targets 30 s with a 40 s maximum. Fewer cuts may improve WER; check peak PSS                           |
| Silero model           | current k2 export (v4 IO names) vs upstream 6.2.3 `silero_vad.onnx`                                                                              | sherpa selects v5 handling for 3-input/2-output models (`silero-vad-model.cc:208-216`). Window must stay 512                                   |

**12. Decode while recording.** Effort M. Largest latency win.

- Feed audio frames to Silero or the energy pause finder during capture.
- When a segment closes, or more than about 15–30 s is pending, cut at the
  quietest point and send that slice to the already-loaded worker while
  recording continues.
- At stop, decode only the tail and join the texts in order. Slices do not
  overlap, so no deduplication is needed. Hotwords keep working, and the
  existing cancel epoch still applies.
- Estimate: stop-to-text latency becomes about one segment's decode,
  independent of total length.

| Project                                                                       | Approach            | Key values                                                                                                             |
| ----------------------------------------------------------------------------- | ------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Handy `chunking.rs`                                                           | VAD-scored cuts     | target 30 s, max 40 s, min 5 s, lookahead 5 s; Silero 0.5 / neg 0.35, min speech 250 ms, min silence 100 ms, pad 30 ms |
| Voxtype `eager.rs`                                                            | Quiet-frame cuts    | `eager_chunk_secs 5.0`; cut at the quietest 20 ms frame within 1.5 s                                                   |
| Vocalinux `recognition_manager.py:4270-4335`                                  | Silence timeout     | `silence_timeout 1.2` s, chosen so no LibriSpeech sentence was cut; `max_segment_seconds 90`                           |
| sherpa main `rust-api-examples/parakeet_tdt_simulate_streaming_microphone.rs` | Simulated streaming | Reference implementation                                                                                               |

Optional live preview, from Sayso `internal/localengine/redecode.go`:

- Every 600 ms, re-decode the uncommitted audio when at least 300 ms is new.
- Commit once the uncommitted audio exceeds 10 s, cutting at the quietest
  100 ms window between 7 s and 2 s before the end.
- Rest for half a decode's duration after a slow decode.
- Pause previews while a final pass runs.

Same model, no extra RAM.

**13. Conservative number formatting (ITN).** Effort M.

- Port Voxtype `src/text/itn.rs` with `text-processing-rs = "0.2"`. Its
  behaviour:
  - Converts only sentences containing a currency, time or unit word, or a
    compound number.
  - Never converts a lone one/two/three/first…tenth/no/a/an (`NEVER_ALONE`).
  - Rejects multi-word rewrites to `@`.
  - Strips leading-zero hours and fixes `3.30 pm` to `3:30` only before am/pm.
- Run it after repetition removal and before user corrections. Make it an
  opt-in "Format numbers" toggle.
- First measure how often Parakeet already writes digits; the agent's guess is
  "often". sherpa `rule_fsts` is not a practical route: only a Chinese number
  FST ships.
- License and dependency size of `text-processing-rs` are not checked.

**14. Better repetition and restart handling.** Effort S–M.

- Minimum: exclude Voxtype's `LEGITIMATE_DOUBLES` from repetition collapse.
- Next: port Voxtype `src/text/restarts.rs`:
  - `MAX_REPARANDUM_TOKENS = 8`
  - `MAX_ANCHOR_TOKENS = 4`
  - `MIN_SINGLE_ANCHOR_LEN = 4`
  - a function-word blocklist
  - editing phrases "scratch that", "I mean", "or rather"
- Capital handoff when a leading filler is removed ("Um, so…" becomes "So…"),
  from Handy `audio_toolkit/text.rs:394-408`. Handy also splits fillers into
  universal and language-gated tiers (`text.rs:308-324`).
- Optional: Voxtype `sentences.rs` joins `. lowercase` breaks and re-decides a
  final `?` from wh-word and auxiliary inversion.

**15. Modifier-release wait before paste.** Effort S–M. Opt-in.

- Poll `EVIOCGKEY` on keyboard devices (those with KEY_A, KEY_Z and KEY_ENTER)
  every 15 ms for up to 1–1.5 s, until Ctrl, Alt, Shift and Super are all up.
- When `/dev/input` is unreadable, fall back to the current 180 ms.
- Do not request the `input` group by default; Voxtype's
  `evdev_listener.rs:10-12` notes the security cost.
- Also sync the terminal list with OpenWhispr `src/helpers/clipboard.js:75-108`:
  ghostty, ptyxis, kgx, org.gnome.console, wezterm, foot, waveterm, cosmicterm
  and others.

**16. Spoken punctuation and commands.** Effort S. Opt-in.

- Sayso `internal/enhance/text.go` (`spokenCommand`): the regex also consumes
  punctuation the model already emitted next to the command, and capitalises
  after `.?!` or a newline.
- Vocalinux `command_processor.py`: "new line", "period", and "scratch that"
  only as a whole utterance.
- Order: corrections, then commands, then filler removal (Vocalinux design
  document).
- Optional: Sayso spelled words ("Katz, that's K A T Z"), with
  `maxReplacedWords 3`.

### Tier 3: experiments and larger features

**17. Learn corrections from user edits.** Effort M–L.

OpenWhispr's mechanism:

- After paste it watches the focused field through AT-SPI
  (`resources/linux-text-monitor.c`), polling every 500 ms for 30 s and
  debouncing 1,500 ms.
- It aligns the pasted and edited text with an LCS and learns substitutions
  (`src/utils/correctionLearner.js`).
- It rejects:
  - edits where more than 50 % of words changed
  - corrected words under 3 characters
  - common words
  - normalised Levenshtein above 0.65

For Hype Flow:

- Store learned pairs as `orig -> corrected` corrections rather than hotwords.
- Show a toast with undo, and keep the feature opt-in.
- Terminals and many Wayland apps expose no AT-SPI text.

**18. Push-to-talk.** Effort M.

- niri delivers no key-release binds (Voxtype `tui/compositor_bindings.rs:442,449`;
  Read). Hold-to-talk therefore needs an evdev listener with inotify hotplug
  (Voxtype `hotkey/evdev_listener.rs`, OpenWhispr `linux-key-listener.c`).
- Ignore autorepeat (value 2) and prefer `/dev/input/by-id`.
- Handy's hold-or-toggle timing: hold 300 ms, release grace 50 ms, debounce
  30 ms.
- The pasted note says niri's next release adds press/release binds. Check the
  installed niri version before building evdev support.

**19. History and recording spool.** Effort M.

- Write PCM to `$XDG_STATE_HOME/hype-flow/spool/<id>.pcm` while recording, and
  delete it after delivery. Offer recovery of leftovers at startup
  (OpenWhispr `recordingSpool.js`).
- A short history with re-transcribe; Handy keeps the last 5 with WAV.
- Privacy defaults: in memory by default (Vocalinux), or 30-day audio retention
  plus an incognito toggle (OpenWhispr, Sayso).

**20. Alternative models and backends.** Effort M–L. Measure WER first.

| Option                                                                                                                   | Benefit                          | Cost                                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------ | -------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Underdog Bark 3.0 int4, 379 MB (Vocalinux `parakeet_model_info.py`)                                                      | About half the model RAM         | Accuracy unknown                                                                                                    |
| Parakeet TDT 110M EN, 136 MB, punctuated (OpenWhispr catalog)                                                            | Ultra-light profile              | Lower accuracy expected                                                                                             |
| whisper.cpp native Parakeet (`include/parakeet.h`, `scripts/quantize-parakeet.sh`: q8_0/q4_0/q4_k/q2_k; Vulkan backends) | Lower PSS, iGPU speed (estimate) | Greedy only, no hotwords                                                                                            |
| OpenVINO EP on the NPU/Arc iGPU (`session.cc:459-505`; `/dev/accel/accel0` present)                                      | Encoder speed-up (estimate)      | Needs an ORT build with the OpenVINO EP plus the OpenVINO runtime; the shipped runtime has only `libonnxruntime.so` |

**21. Upgrade sherpa-onnx to main (2.0.3).** Effort M.

Fixes relevant to Hype Flow's path (Read):

- Offline NeMo feature log guard `log(x + FLT_EPSILON/2)` (#3999).
- NeMo beam-search rewrite with hypothesis merging and per-stream
  `length_reward` via `SetOption("length_reward","0.5")`, which "keeps beam
  search from dropping speech after a pause".
- VAD `Flush` no longer drops the partial last window (#4147).

Upgrade risk: `bindings.rs` must first append `ctc_fst_decoder_config` to
`SherpaOnnxOfflineRecognizerConfig` and `voxlock` to
`SherpaOnnxOfflineSpeechDenoiserModelConfig`. Otherwise C reads past the Rust
structs. Adopt only after an A/B on the harness.

**22. Optional local LLM cleanup.** Effort L. Off by default.

Separate `llama-server` process:

- `-m <1–3B>-Q4_K_M.gguf -lm mmap -c 2048 -np 1`
- `--sleep-idle-seconds N` to match idle unload
- `--spec-type ngram-simple --spec-draft-n-max 64`, since the output mostly
  copies the input (`docs/speculative.md:175-187`)
- Constrain output with `--json-schema` or a GBNF grammar

Guardrails:

- Voxtype edit-list validator (`src/output/post_process.rs:542-608`):
  - each edit's find text must be unique
  - `new ≤ 2·old + 8`
  - Levenshtein ≤ `max(old/2, 4)`
  - total growth ≤ `max(len/5, 8)`
  - no overlapping edits
- OpenWhispr rejects doubled output (`cleanupOutput.ts`) and wraps input in
  `<transcript>` with "Output only the cleaned transcript."
- Strip `<think>` blocks. Fall back to raw text on timeout (30–60 s in the
  references).
- Sayso's local cleanup models are about 0.5–0.64 GB (Qwen3-0.6B GGUF).

The existing transcript hook already allows this externally.

**23. Parallel segment decode.** Effort M.

`SherpaOnnxDecodeMultipleOfflineStreams` (`c-api.h:1458`) can batch the pieces.
OpenWhispr uses intra-op threads = `max(1, min(4, floor(cpus × 0.75)))` with up
to 3 concurrent streams. Gain is modest once item 12 exists.

**24. Per-stream hotwords.** Effort M.

`SherpaOnnxCreateOfflineStreamWithHotwords` (`c-api.h:1332`) builds a
per-stream context graph. Vocabulary edits would then apply without reloading
the 624 MB model. Beam search only.

---

## 7. Not recommended

| Item                                                                                                                                                                                          | Reason                                                                                                                                                                                                          |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Whisper hallucination thresholds (`no_speech_thold`, `logprob_thold`, `entropy_thold`, compression ratio, temperature fallback), initial prompts, `[BLANK_AUDIO]` and vocabulary-echo filters | Specific to autoregressive Whisper; Parakeet transducers have no prompt                                                                                                                                         |
| True streaming Parakeet Unified (`online-recognizer-transducer-nemo-parakeet-unified-impl.h`)                                                                                                 | Each chunk re-encodes about 70 left-context frames (5.6 s). With the 1120 ms preset that is about 12× offline compute (estimate). Greedy only, so hotwords are lost. Needs a separate ~630 MB streaming package |
| sherpa punctuation models (`ct_transformer`, `cnn_bilstm`)                                                                                                                                    | Parakeet already emits punctuation and case                                                                                                                                                                     |
| sherpa `rule_fsts` for English ITN                                                                                                                                                            | Only `itn_zh_number.fst` ships; compiling NeMo grammars for sherpa is unverified                                                                                                                                |
| Homophone replacer                                                                                                                                                                            | Chinese only; ASCII words pass through (`homophone-replacer.cc:170-180`)                                                                                                                                        |
| LM shallow fusion, `temperature`                                                                                                                                                              | Not used by the NeMo transducer path                                                                                                                                                                            |
| `SherpaOnnxOfflineRecognizerSetConfig`                                                                                                                                                        | No effect for the NeMo implementation, which keeps its own config (`offline-recognizer-impl.cc:935`)                                                                                                            |
| cpal / rubato / custom resamplers                                                                                                                                                             | `pw-record` already delivers 16 kHz mono                                                                                                                                                                        |
| Voxtype signal-and-file IPC, Sussurro socket                                                                                                                                                  | GApplication actions are cleaner                                                                                                                                                                                |
| Per-app profiles                                                                                                                                                                              | Voxtype selects profiles only by CLI flag; there is little evidence of value                                                                                                                                    |
| whisperX forced alignment                                                                                                                                                                     | TDT/RNNT already gives token timestamps                                                                                                                                                                         |
| Voxtype disfluency tagger                                                                                                                                                                     | Requires their trained `VXDFL001` weights                                                                                                                                                                       |
| Separate streaming preview model (Sayso Kroko)                                                                                                                                                | A second model costs RAM; the re-decode preview reuses the main model                                                                                                                                           |

---

## 8. Per-project findings

### 8.1 Handy (Rust/Tauri)

Relevant files (`src-tauri/src/`):

| File                                                                      | Content                                                                                                                                | Relevance                                                                                |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `audio_toolkit/text.rs`                                                   | Fuzzy custom words, filler removal, stutter collapse                                                                                   | Hype Flow's matcher is a port. New: universal vs language-gated fillers, capital handoff |
| `audio_toolkit/vad/{mod,smoothed,silero,earshot}.rs`                      | Per-frame VAD during capture: prefill 450 ms, hangover 450 ms, onset 60 ms, Silero 0.3                                                 | Needed for item 12                                                                       |
| `audio_toolkit/chunking.rs`                                               | VAD-scored cut planner (constants in item 12)                                                                                          | Item 12                                                                                  |
| `audio_toolkit/audio/sink.rs`                                             | Inline VAD, digital-silence detection                                                                                                  | Hype Flow has silent-mic detection                                                       |
| `managers/audio.rs`                                                       | Mic lifecycle, mute while recording (`wpctl set-mute`), `extra_recording_buffer_ms` (default 0)                                        | Optional                                                                                 |
| `managers/transcription.rs`, `transcription/{policy,live,chunked,job}.rs` | Decode policy, live decode, idle unload, punctuation-only result → empty (`:1745-1750`), `catch_unwind` fallback to raw text (`:1782`) | Items 2, 12                                                                              |
| `managers/history.rs`, `commands/history.rs`                              | SQLite history plus WAV, retry, retention                                                                                              | Item 19                                                                                  |
| `memory.rs`                                                               | `mallopt` / `malloc_trim`                                                                                                              | Item 5                                                                                   |
| `transcription_coordinator.rs`, `shortcut/*`                              | Toggle / push-to-talk / hold-or-toggle (hold 300 ms, grace 50 ms, debounce 30 ms)                                                      | Item 18                                                                                  |
| `llm_client.rs`, `settings.rs`                                            | LLM cleanup: 60 s timeout falls back to raw text; JSON schema `{transcription}`; `<think>` stripping                                   | Item 22                                                                                  |
| `cli.rs`                                                                  | `--transcribe-file`, `--repeat N` headless benchmark                                                                                   | Hype Flow has `evaluate.rs`                                                              |
| `utils.rs` `redact_text`                                                  | Transcript redaction in release logs                                                                                                   | Check Hype Flow app logs                                                                 |
| `catalog.json`                                                            | parakeet-unified-en-0.6b FLEURS en 3.99 % WER, CPU 13.955 (unit assumed ×real-time)                                                    | Confirms the model choice                                                                |

Skipped: the TSX UI (grepped only), generated bindings, Nix, workflows, NSIS,
icons, overlay, tray, macOS/Windows paste, language ID.

### 8.2 OpenSuperWhisper (Swift, macOS)

| File                                                                                    | Content                                                                                                      | Relevance                                            |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------- |
| `Engines/WhisperEngine.swift`                                                           | VAD gate: no speech → empty (`:195-200`); segment stitching with 0.1 s overlap plus 0.1 s zeros (`:387-410`) | Items 2, 6                                           |
| `Engines/FluidAudioEngine.swift`                                                        | Parakeet v2/v3 via CoreML, fresh decoder state per run                                                       | Hype Flow equivalent: a fresh offline stream per run |
| `AudioRecorder.swift`                                                                   | Stop tail 0.25 s (`:19,240`), minimum duration 1.0 s (`:15,255`)                                             | Items 2, 7                                           |
| `Indicator/IndicatorWindow.swift`, `TranscriptionQueue.swift`, `Models/Recording.swift` | Queue dictation when busy; keep failed audio                                                                 | Items 8, 19                                          |
| `OpenSuperWhisperTests/LongFormTranscriptionTests.swift`                                | Checkpoint phrases near 30/60/90 s                                                                           | Item 9                                               |

Skipped: the whisper.cpp and asian-autocorrect submodules, xcodeproj, model
binary, `agent/` (an issue bot), and the macOS-only monitors.

### 8.3 OpenWhispr (Electron)

| File                                                                                     | Content                                                                                  | Relevance                                |
| ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------- |
| `src/utils/sherpaHotwords.js`                                                            | Per-model hotword scores; Unified disabled; 100 cap; `:` filter; dedupe                  | Item 11, defect 4.4                      |
| `src/helpers/localSpeechGate.js`, `audioManager.js:1729-1748,2350-2370`                  | RMS/peak speech gate                                                                     | Item 2                                   |
| `src/helpers/parakeetServer.js`                                                          | `SILENCE_RMS_THRESHOLD = 0.001`, empty-decode retry, parallel segments                   | Items 2, 23                              |
| `src/helpers/parakeetWsServer.js`                                                        | Thread formula, warm-up decode (`:416-428`), hotwords file                               | Items 4, 23                              |
| `src/helpers/parakeetSegmentation.js`                                                    | Pause splitter                                                                           | Already ported as `segments.rs`          |
| `model-registry/registry/parakeet.json`                                                  | TDT-0.6b-v3, unified-en, TDT-110m-en, Nemotron streaming (560 ms), Orukeet               | Item 20                                  |
| `src/utils/correctionLearner.js`, `textEditMonitor.js`, `resources/linux-text-monitor.c` | Auto-learn from edits                                                                    | Item 17                                  |
| `src/helpers/clipboard.js`, `resources/linux-fast-paste.c`                               | Restore delays (Linux 800 ms, KDE Wayland 1200 ms), terminal list, modifier wait 1500 ms | Item 15                                  |
| `resources/linux-key-listener.c`, `hyprlandShortcut.js:700`, `hotkeyRepeatGate.js`       | evdev push-to-talk; Hyprland `bindrt`; 600 ms repeat gate                                | Item 18                                  |
| `src/helpers/recordingSpool.js`, `retentionSettings.js`, `discardedRecording.js`         | Crash spool, 30-day audio retention, discarded takes                                     | Item 19                                  |
| `transcriptionTimeout.js`                                                                | Timeout floor 300,000 ms plus 10,000 ms per audio second                                 | Low priority; Hype Flow has a flat 600 s |
| `cleanupOutput.ts`, `config/prompts/index.ts:41`                                         | LLM output guards                                                                        | Item 22                                  |
| `src/utils/snippets.ts`                                                                  | Spoken trigger expansion                                                                 | Covered by corrections                   |
| `whisperVadConfig.js`                                                                    | Dictation VAD off by default; Silero 0.5 / 250 / 200 / 30 s / pad 100 ms                 | Data point                               |
| `settingsStore.ts:1941`                                                                  | `telemetryEnabled` false by default                                                      | Hype Flow is offline                     |

Skipped: `openwhispr-mobile/` (998 files), `src/components/` (593 files),
cloud/enterprise/account/billing, connectors, meetings and diarization,
notes/agents, native macOS/Windows, docs, most tests.

### 8.4 Voxtype (Rust, Linux/Wayland)

| File                                                       | Content                                                                                                                                                         | Relevance                                        |
| ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| `src/text/mod.rs`                                          | Cleanup order: vocab, filler, replacements, spoken punctuation, disfluency, restarts, punctuation model, sentence repair, ITN, replacements again, smart submit | Order reference                                  |
| `src/text/itn.rs`                                          | Conservative ITN (`:84-87,109-131,135-147,262-288`)                                                                                                             | Item 13                                          |
| `src/text/vocabulary.rs`                                   | Strict DoubleMetaphone fuzzy gate                                                                                                                               | Item 1                                           |
| `src/text/restarts.rs`                                     | Restart and stutter removal (`:26-66`)                                                                                                                          | Item 14                                          |
| `src/text/sentences.rs`                                    | Break repair, `?` decision                                                                                                                                      | Item 14                                          |
| `src/transcribe/parakeet.rs`, `config/engines/parakeet.rs` | parakeet-rs; `max_window_secs 240` (TDT attention aborts near 295 s); overlap 5.0; streaming chunk 0.5 / left 1.5 / right 0.5                                   | Reference; Hype Flow's 15 s cuts are safe        |
| `src/transcribe/window.rs`                                 | Overlap windows plus word-overlap stitch                                                                                                                        | Alternative to pause cuts                        |
| `src/eager.rs`                                             | Eager chunk transcription                                                                                                                                       | Item 12                                          |
| `src/vad/energy.rs`, `src/config/vad.rs`                   | Adaptive energy VAD and silence gate                                                                                                                            | Item 2                                           |
| `src/audio/input_guard.rs`                                 | Dead-mic detection (150 Hz high-pass, dBFS)                                                                                                                     | Hype Flow has partial coverage                   |
| `src/output/modifier_guard.rs`                             | EVIOCGKEY modifier wait, 15 ms poll                                                                                                                             | Item 15                                          |
| `src/output/clipboard_restore.rs`                          | 2 s read deadline, process-group kill, newer copy wins                                                                                                          | Equal to Hype Flow                               |
| `src/output/fallback.rs`                                   | Notifies the delivery target; keeps undelivered text                                                                                                            | Partial                                          |
| `src/output/post_process.rs`, `src/llm/mod.rs`             | LLM edit-list validator, 30,000 ms timeout, streaming `<think>` stripper (`:549`)                                                                               | Item 22                                          |
| `src/failed_recording.rs`                                  | `KEEP = 20`, 0700/0600                                                                                                                                          | Item 8                                           |
| `src/history/*`                                            | JSONL day files, FTS, replacement suggestions (`MAX_FROM_WORDS 4`, `MAX_TO_WORDS 6`)                                                                            | Item 19                                          |
| `src/hotkey/evdev_listener.rs`                             | evdev push-to-talk with inotify hotplug                                                                                                                         | Item 18                                          |
| `packaging/systemd/voxtype.service`                        | `PartOf/After=graphical-session.target`, `After=pipewire.service`, `Restart=on-failure`, `RestartSec=5`                                                         | Optional; Hype Flow uses niri `spawn-at-startup` |
| `tests/fixtures/vad/*.wav`                                 | silence_50ms, white_noise, tone_440hz, speech_quiet                                                                                                             | Item 9                                           |
| `config/text.rs:181-192`                                   | Default fillers: uh um er ah eh hmm hm mm mhm (not "like")                                                                                                      | Hype Flow equivalent                             |

Skipped: meeting mode, TUI/OSD/quickshell/omarchy, macOS, website, tray, setup
and GPU probing, Dockerfiles, cloud engines, and the internals of non-Parakeet
engines. No benchmarks exist in the repository.

### 8.5 Sussurro (Go)

| File                                                       | Content                                                                                                                        | Relevance    |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------------ |
| `internal/asr/whisper.go`, `vad_integration_test.go:40-94` | VAD regression tests with a real model                                                                                         | Item 9       |
| `internal/pipeline/pipeline.go:150`                        | 300 ms "Too short to transcribe"                                                                                               | Item 2       |
| `internal/llm/cleanup.go`, `llama.go`                      | Chunked prompts, 512-token cap, hallucination-marker stripping, seam-only punctuation repair, recapitalising at deletion seams | Items 14, 22 |
| `internal/llm/correction.go`                               | Bounded LLM validator                                                                                                          | Item 22      |
| `internal/trigger/*`                                       | Unix socket `$XDG_RUNTIME_DIR/sussurro.sock` with toggle/press/release/cancel                                                  | Not needed   |
| `internal/input/*`                                         | evdev chord; ignores autorepeat value 2; prefers `/dev/input/by-id`                                                            | Item 18      |

Skipped: X11 context capture, the GTK3/webview UI, packaging.

### 8.6 Vocalinux (Python)

| File                                                                | Content                                                                                                                                                                           | Relevance                       |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------- |
| `speech_recognition/recognition_manager.py`                         | Segment decode while recording (`:4270-4335`), non-speech filter (`:325-388`), `<unk>` strip (`:2476`), stop-sound guard, Silero threshold `0.8 − (s − 1) × 0.125` (default 0.55) | Items 2, 7, 12                  |
| `command_processor.py`, `command_phrases.py`                        | Voice commands; punctuation aliases in 9 languages                                                                                                                                | Item 16                         |
| `custom_dictionary.py`, `docs/CUSTOM_DICTIONARY_DESIGN.md`          | Terms vs corrections; corrections before commands; "No prompt-bias API" for Parakeet                                                                                              | Item 16                         |
| `utils/parakeet_model_info.py`                                      | Alternative sherpa transducers (Bark int4 379 MB, Orukeet)                                                                                                                        | Item 20                         |
| `text_injection/*`                                                  | xdotool/wtype/ydotool/IBus/portal, terminal detection, image-safe clipboard restore                                                                                               | Mostly covered                  |
| `playback_ducker.py`                                                | Duck to 20 % with `VOLUME_MATCH_TOLERANCE 0.015` and a crash-recovery file                                                                                                        | Optional alternative to pausing |
| `suspend_handler.py`, `auto_pause_monitor.py`, `model_keepalive.py` | logind `PrepareForSleep`, game pause, keepalive                                                                                                                                   | Low priority                    |
| `docs/benchmarks/README.md`                                         | Why 1.2 s silence timeout                                                                                                                                                         | Item 12                         |

Skipped: packaging, snap/flatpak, `install.d`, sounds, `gateway_embed`, the
settings dialog (10k lines), tray, update checker; the 157 tests by name only.

### 8.7 Sayso (Go engine plus Rust crates; sherpa-onnx 1.13.8 C API)

| File                                                        | Content                                                                                                                                                                                    | Relevance               |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------- |
| `internal/localengine/redecode.go`                          | Re-decode live preview (`:12-21,163-172,329-334`)                                                                                                                                          | Item 12                 |
| `internal/localengine/sherpa.go`                            | Offline and online recognizers; threads `min(cpu,8)` final, `min(cpu,2)` preview; streaming finish pads 2 s ("0.8 s loses the last words"); dictionary ignored on catalog models           | Item 11                 |
| `internal/localengine/audio.go`, `spec.go`                  | Quietest-window cuts (100 ms window, 20 ms hop); Parakeet TDT chunk 60 s, cut in the quietest 200 ms window of the last 5 s                                                                | Item 11                 |
| `internal/localengine/vad.go`                               | Threshold 0.5, min silence 0.5, min speech 0.25, 30 s parts                                                                                                                                | Data point              |
| `internal/localengine/text.go`                              | Committed vs tentative partial text                                                                                                                                                        | Item 12                 |
| `internal/enhance/text.go`                                  | Spoken punctuation, spelled words, `WordsHeard` (joins up to 4 spoken words, distance ≤ len/3)                                                                                             | Item 16                 |
| `internal/dictation/machine.go:247-341`                     | Undo after cancel (keeps audio)                                                                                                                                                            | Item 19                 |
| `internal/config/config.go:54`                              | `MinDurationMs: 300`                                                                                                                                                                       | Item 2                  |
| `internal/platform/insert_linux.go:300-345`                 | Shift+Insert default; 50 ms wait for clipboard managers; restore after 450 ms if unchanged                                                                                                 | Consider the 50 ms wait |
| `docs/benchmark.md`, `scripts/benchmark/{prepare,score}.py` | 7 Open ASR sets × 200 clips, fixed seed; clips under 1 s padded; Whisper normalizer plus jiwer. Linux 7700X: Parakeet TDT v2 sherpa CPU 6.72 WER at 40×. parakeet-unified-en 6.29 (CoreML) | Item 9                  |
| `docs/cleanup-models.md`                                    | S1-mini 484 MB / Cleanup-1 639 MB; medians 68 / 82 ms on M5 Pro                                                                                                                            | Item 22                 |

Skipped: translations (~400 YAML), the TypeScript SDK, macOS Swift,
Windows/macOS platform code, the extension bridge, cloud providers, UI,
packaging, release scripts.

### 8.8 sherpa-onnx 1.13.8 (pinned) and main 2.0.3

| Capability                                                 | Evidence                                                                                                                                                                                                                 | Status in Hype Flow              | Decision             |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------- | -------------------- |
| `model_type = "nemo_transducer"`                           | `offline-recognizer-impl.cc:295-300`                                                                                                                                                                                     | Unset                            | Adopt (item 3)       |
| Provider config `"cpu:<cfg>"`                              | `session.cc:85-226`                                                                                                                                                                                                      | Unused                           | Measure (item 11)    |
| `ys_log_probs`, `timestamps`, `tokens` in JSON             | greedy decoder `:60-90,141-171`; `offline-stream.cc:430-510`                                                                                                                                                             | Ignored                          | Adopt (item 10)      |
| `blank_penalty`                                            | `offline-recognizer.cc:44`; applied in greedy and beam                                                                                                                                                                   | Field bound, 0                   | Sweep (item 11)      |
| Per-stream hotwords                                        | `c-api.h:1332`                                                                                                                                                                                                           | Not bound                        | Maybe (item 24)      |
| `DecodeMultipleOfflineStreams`                             | `c-api.h:1458`                                                                                                                                                                                                           | Not used                         | Maybe (item 23)      |
| Streaming Parakeet Unified                                 | `online-recognizer-impl.cc:53-58,118-120`; presets 70/7/7, 70/2/5, 70/1/2 frames (`export_onnx_streaming.py:15-17`)                                                                                                      | No                               | Not now              |
| Endpoint detection                                         | `c-api.h:361-370`; defaults 2.4 / 1.2 / 20 s                                                                                                                                                                             | No                               | Only with streaming  |
| Punctuation, ITN FSTs, homophone replacer, LM, temperature | See section 7                                                                                                                                                                                                            | No                               | Skip                 |
| VAD `neg_threshold`                                        | In C++ (`silero-vad-model-config.h:33-39`), not in the C struct                                                                                                                                                          | Implicit `max(thr − 0.15, 0.01)` | n/a                  |
| VAD semantics                                              | Segment start at tail − 2·window − min_speech (`voice-activity-detector.cc:95-99`); end at tail − min_silence (`:112`); above `max_speech` switches to threshold 0.9 and min silence 0.1 s (`:227-229`); no `speech_pad` | Hype Flow adds its own padding   | Keep                 |
| OpenVINO EP                                                | `session.cc:459-505`, default `device_type = NPU`                                                                                                                                                                        | Not shipped                      | Experiment (item 20) |
| Threads                                                    | Intra- and inter-op both set from `num_threads` (`session.cc:150-152`)                                                                                                                                                   | `min(cpus, 4)`                   | Sweep                |

Only in main: the NeMo feature log guard, the beam-search rewrite with
`length_reward`, the VAD `Flush` fix, streaming Parakeet Unified fixes (#4133),
streaming NeMo beam plus hotwords for cache-aware models (#3895, not Unified),
VoxLock target-speaker isolation, and new Parakeet TDT Ultra/Redux exports. See
item 21.

### 8.9 Silero VAD (6.2.3)

| Item                                       | Detail                                                                                                                                                        |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Upstream defaults (`utils_vad.py:283-296`) | threshold 0.5, `neg_threshold = threshold − 0.15`, `min_speech_duration_ms` 250, `min_silence_duration_ms` 100, `speech_pad_ms` 30, `max_speech_duration_s` ∞ |
| Long-speech handling (`:497-519,560-572`)  | Cut at the longest pause; split padding                                                                                                                       |
| Model                                      | `data/silero_vad.onnx` 2.3 MB (v5/v6). The installed k2 export is 629 KB with `x/h/c` inputs (v4 lineage inferred)                                            |
| Hype Flow values                           | 0.3 / 0.45 s / 0.06 s / 30 s, plus 450/200 ms padding: reasonable for dictation. A/B the v6 model (item 11)                                                   |

### 8.10 whisper.cpp

| Item                   | Evidence                                                                                                                                                                               | Relevance             |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- |
| Native Parakeet (ggml) | `include/parakeet.h`, `src/parakeet.cpp`; greedy only; token `p`, `plog`, `is_word_start`, `duration_value`; local attention above 8192 frames (`:138-141`); GPU backends (`:827-833`) | Item 20               |
| Parakeet quantization  | `scripts/quantize-parakeet.sh`: q8_0, q4_0, q4_k, q2_k; `tests/librispeech-parakeet/`                                                                                                  | Item 20               |
| VAD concatenation      | `src/whisper.cpp:6865-6930`: 0.1 s overlap plus 0.1 s zeros                                                                                                                            | Item 6                |
| Streaming example      | `examples/stream/stream.cpp:20-34`: `step_ms=3000`, `length_ms=10000`, `keep_ms=200`, `vad_thold=0.6`, `freq_thold=100.0`                                                              | Reference for item 12 |
| Whisper thresholds     | `temperature_inc=0.2`, `entropy_thold=2.4`, `logprob_thold=-1.0`, `no_speech_thold=0.6`                                                                                                | Not applicable        |

### 8.11 faster-whisper and whisperX

| Item                        | Evidence                                                                                                                                                                                                                                      | Relevance               |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| Hallucination anomaly score | `faster_whisper/transcribe.py:1278-1296`                                                                                                                                                                                                      | Adapt (item 10)         |
| No-speech handling          | `transcribe.py:921-929`: no VAD segments, empty output                                                                                                                                                                                        | Item 2                  |
| `VadOptions`                | `vad.py:41-48`: threshold 0.5, `neg_threshold = max(thr − 0.15, 0.01)`, `min_silence_duration_ms` 2000, `speech_pad_ms` 400, `min_silence_at_max_speech` 98, `use_max_poss_sil_at_max_speech` True; split at the longest silence (`:113-139`) | Reference               |
| whisperX `merge_chunks`     | `whisperx/vads/vad.py:20-53`: `chunk_size=30`, `vad_onset=0.500`, `vad_offset=0.363`                                                                                                                                                          | Batch packing (item 23) |
| whisperX alignment          | wav2vec2 forced alignment                                                                                                                                                                                                                     | Not needed              |

### 8.12 NeMo (and `Speech`)

| Item                         | Evidence                                                                                                                                                                                                              | Relevance                                                                       |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Confidence estimation        | `asr_confidence_utils.py:91-95,180-186`: entropy, `tsallis`, `alpha=0.33`, `exp` norm, `min` aggregation (streaming YAML: `alpha 0.5`, `mean`)                                                                        | Item 10                                                                         |
| Boosting tree                | `context_biasing/boosting_graph_batched.py:53-95,645-690`: `context_score=1.0`, `depth_scaling=2.0`, `score_per_phrase=0.0`, `bpe_mode` (`case_insensitive` in streaming), `num_of_transcriptions=5`, `bpe_alpha=0.3` | Case variants already handled; alternative BPE splits would need a sherpa patch |
| Keyword F-score              | `context_biasing_utils.py:152-230`                                                                                                                                                                                    | Item 9                                                                          |
| Buffered long-audio decoding | `speech_to_text_streaming_infer_rnnt.py:36-38` ("10-10-5" long files, "10-2-2" streaming); `buffered_rnnt.yaml` (`chunk_size 4.8`, padding 1.6/1.6; default model `nvidia/parakeet-unified-en-0.6b`)                  | Alternative to pause cuts                                                       |
| WER normalization            | `speech_to_text_eval.py:159-167`: `ignore_capitalization`, `ignore_punctuation`, `separate_punctuation`                                                                                                               | Item 9                                                                          |
| Augmentation                 | `perturb.py:457-476`: `NoisePerturbation(min_snr_db=10, max_snr_db=50)`, speed, RIR, transcode                                                                                                                        | Item 9                                                                          |
| ITN                          | `inference/itn/inverse_normalizer.py` (pynini grammars, `max_number_of_permutations_per_split 729`)                                                                                                                   | Use the Voxtype port instead (item 13)                                          |

### 8.13 llama.cpp

| Item                                                      | Evidence                                                                                                                                                                             | Relevance |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------- |
| Server flags                                              | `tools/server/README.md`: load mode (`:74`), KV cache type (`:71`), grammar and JSON schema (`:153-156`), `--sleep-idle-seconds` default −1 (`:252`), speculative types (`:281-287`) | Item 22   |
| `ngram-simple` speculative decoding for rewrite workloads | `docs/speculative.md:175-187`                                                                                                                                                        | Item 22   |
| Quantization                                              | `tools/quantize/README.md:165-168`: Q4_K_M at 4.8944 bpw                                                                                                                             | Item 22   |

---

## 9. Mapping to the optimization checklist

Status of each item from the supplied Rust optimization checklist.

### Accuracy

| Item                                    | Status                                               | Action                                 |
| --------------------------------------- | ---------------------------------------------------- | -------------------------------------- |
| English-only decoding                   | Done                                                 | —                                      |
| Domain vocabulary                       | Done, with defect 4.1                                | Items 1, 11                            |
| VAD tuning                              | Partial                                              | Items 2, 6, 11                         |
| Audio quality                           | Partial (GTCRN denoise)                              | Measure with augmented corpus (item 9) |
| Decoding strategy                       | Not compared                                         | Item 11                                |
| Hallucination protection                | Missing                                              | Items 2, 10                            |
| Text cleanup that preserves identifiers | Partial (protected spans exist; fuzzy over-corrects) | Items 1, 10                            |

### Latency

| Item                      | Status                              | Action                           |
| ------------------------- | ----------------------------------- | -------------------------------- |
| Persistent model sessions | Done                                | —                                |
| Streaming partials        | Missing                             | Item 12 (re-decode preview)      |
| Incremental inference     | Missing                             | Item 12                          |
| Endpoint detection        | Missing                             | Not needed for toggle mode       |
| Dedicated workers         | Done                                | —                                |
| Inference parallelism     | Fixed at `min(cpus, 4)`             | Item 11                          |
| Avoid unnecessary passes  | Done (no LLM, optional VAD/denoise) | Item 3 removes the startup probe |

### RAM and storage

| Item                | Status                   | Action                                         |
| ------------------- | ------------------------ | ---------------------------------------------- |
| Model quantization  | int8                     | Item 20                                        |
| Single-model mode   | Done                     | —                                              |
| Lazy initialization | Done                     | —                                              |
| Bounded buffers     | Done (300 s cap)         | Item 6 (VAD buffer)                            |
| Runtime sharing     | Done (one worker)        | —                                              |
| Compact packaging   | `strip = true`, thin LTO | Optional `lto = "fat"`, `-C target-cpu=native` |
| Idle behaviour      | Done                     | —                                              |

### Reliability and Linux

| Item                        | Status                                         | Action                                 |
| --------------------------- | ---------------------------------------------- | -------------------------------------- |
| Compositor-native shortcuts | Done (niri binds)                              | —                                      |
| Audio device changes        | Partial (first-audio timeout)                  | Not covered by the references in depth |
| Safe text injection         | Done                                           | Item 15 (modifier wait)                |
| Cancellation                | Done                                           | —                                      |
| Service lifecycle           | niri `spawn-at-startup`                        | Optional systemd user unit (Voxtype)   |
| Privacy                     | Done (no network, transcript deletion on exit) | Keep history opt-in (item 19)          |

### Memory profiles

| Profile            | Suggested composition                                                                                    |
| ------------------ | -------------------------------------------------------------------------------------------------------- |
| Ultra-light        | Smaller model (item 20), no VAD or denoise, greedy, aggressive unload                                    |
| Balanced (default) | Current model, greedy unless item 11 proves hotwords help, VAD with silence guard, deterministic cleanup |
| Accuracy-first     | Beam with hotwords if measured useful, GTCRN, ITN, optional LLM (item 22)                                |

---

## 10. Measurement plan

Change one variable at a time with fixed audio, hardware power profile, model,
build and decoding parameters.

| Metric                                                                  | Use                         |
| ----------------------------------------------------------------------- | --------------------------- |
| WER/CER (normalised)                                                    | Items 6, 11, 12, 13, 20, 21 |
| Keyword precision/recall/F1, and false insertions on non-keyword speech | Items 1, 10, 11             |
| Empty-output rate on silence, cough, click and keyboard clips           | Items 2, 10                 |
| p50/p95 stop-to-paste latency                                           | Items 3, 4, 12, 23          |
| Real-time factor                                                        | Items 11, 20                |
| Idle, loaded and peak process-tree PSS                                  | Items 3, 5, 6, 11, 20       |
| Cold-start time                                                         | Items 3, 4                  |

Suggested order: **9, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12**, then the rest by
need.

---

## 11. Open questions

- Does vocabulary boosting improve keyword recall on parakeet-unified-en at
  any score, or only add latency? See 4.4 and item 11.
- How often does Parakeet already write numbers as digits? This decides
  item 13.
- Is the stop cue audible in recordings through laptop speakers? See 4.3.
- Does the installed niri support press/release binds? This decides item 18.
- What are the real PSS and latency effects of `EnableCpuMemArena=0`,
  `EnableMemPattern=0` and `malloc_trim`?
- Is the installed Silero export v4, and does v6 change recall on quiet speech?
- What WER and keyword F1 do Bark int4 and the whisper.cpp quantized Parakeet
  achieve?
- Does a 30–60 s maximum piece length improve WER near cuts without raising
  peak PSS too far?

## Implementation follow-through

See [implementation-status.md](implementation-status.md) for the recommendation
mapping, public benchmark results, and remaining validation and research items.
The [evaluation summaries](eval-results/README.md) include the matched
1,400-clip model comparison and the results of the vocabulary and VAD sweeps.
The [CTC pause checks](implementation-status.md#ctc-pause-preservation) track
leading-silence, interior-pause, and long-form behavior.

### Remaining work

- **5, 9–12, 15.** Repeat the exploratory decoder, VAD and memory comparisons
  on the default CTC model. Measure runtime toggles one at a time, compare the
  Silero exports, and validate confidence thresholds on models that provide
  token probabilities. Test live capture underruns and stop-to-paste latency
  in VS Code, a terminal, and a browser. Check for stop-cue leakage while the
  speakers and microphone are active.
- **17, 18.** Edit learning still needs reliable AT-SPI text access. The
  installed niri 26.04 rejects press/release bindings, and this account has
  no readable evdev devices for verifying a hold-to-talk fallback.
- **20–24.** The 110M TDT and CTC profiles ship, with CTC selected by default.
  Additional engines such as Bark int4 and OpenVINO remain research. A sherpa
  upgrade still needs matching C bindings and a paired evaluation. Local LLM
  cleanup, batch decoding, and per-stream hotwords remain optional.
