# Evaluation summaries

Only aggregate JSON summaries belong here. Keep recordings, manifests and
per-case transcripts outside the repository, especially personal dictations.

Run a reproducible comparison after building the workspace:

```sh
make release
python3 tools/compare-eval.py /external/corpus/manifest.jsonl docs/eval.toml --repeats 3
```

Each configuration produces strict, published Whisper English and formatted
WER, substitution/insertion/deletion counts, keyword precision/recall/F1,
length/tag breakdowns, minimum-of-N per-case processing p50/p95, audio/processing
ratio and worker peak RSS. The table prints selected metrics; JSON retains all
metrics, the effective configuration, manifest SHA-256 and streaming SHA-256/size
fingerprints for the worker and all enabled model assets. Fingerprinting happens
once outside timing and warms model pages, so load time is not a cold-cache claim. `VmHWM` is the worker's cumulative
high-water mark across the run, not per-case incremental memory. Startup time
is reported separately. Processing latency includes decode and cleanup, not
capture, paste, audio loading or scoring. `real_time_factor` follows this plan's
specified audio-seconds/processing-seconds ratio (higher is faster); many other
benchmarks use the reciprocal, so compare carefully.

`--set 'threads=2'`, `--set 'vad_threshold=0.4'`,
`--set 'noise_suppression=true'`, `--set 'blank_penalty=0.5'` and
`--set 'max_active_paths=8'` override individual TOML settings without modifying
preferences. Use `--normalizer whisper --max-wer 0.1` for a normalized accuracy
gate. Both raw and cleaned results are always reported.

The Whisper scorer embeds the pinned MIT-licensed upstream English normalizer
and spelling map. It runs an evaluation-only Python standard-library helper.
This deliberately preserves the upstream number grammar rather than approximating
it in Rust. The application and worker remain native Rust. Python startup and
scoring are outside processing-time measurements.

Prepare datasets outside the repository:

```sh
uv run tools/prepare-eval.py public --out ~/.cache/hype-flow-eval/public
python3 tools/prepare-eval.py augment --manifest /external/manifest.jsonl --out /external/augmented
python3 tools/prepare-eval.py media --manifest /external/manifest.jsonl --out /external/media
python3 tools/prepare-eval.py long --manifest /external/manifest.jsonl --out /external/long
python3 tools/prepare-eval.py control --manifest /external/manifest.jsonl --keywords 'OpenAI,Rust' --out /external/control
HYPE_FLOW_MODEL_TEST=1 python3 tools/check-model.py /external/manifest.jsonl
```

Public sampling uses fixed seed 20261015, 200 clips per each of seven English
Open ASR sets, and cached Parquet downloads/WAV files for resumability. The
Hugging Face dataset is pinned to immutable revision
`de79e57adf9983993bcc72586d7313460825f5eb`; `--revision` accepts another
40-character commit SHA. Metadata records revision and seed. Cached shards and
WAVs are namespaced by revision. Preserve the manifest and shards to reproduce a corpus.
Public preparation requires optional `pyarrow`, `numpy`, `soundfile` and `soxr`;
`uv run` installs them for that script only. Other corpus commands use the
standard library. Augmentation adds deterministic 0–5 second boundary silence
and Gaussian noise at 0/5/10/15 dB SNR. Long-form stitching retains complete
reference clips, so durations meet or exceed 60/120/180 seconds. Checkpoints
record each component clip's start and reference; select shorter boundary
phrases when validating actual recognizer output.

The `media` command requires ffmpeg with libmp3lame/libopus. It adds speed
variants at 0.9×/1.1×, MP3 at 64 kbit/s and Opus at 32 kbit/s, each decoded
back to mono 16 kHz PCM16 WAV. References remain unchanged. MP3 can retain
encoder-frame padding; duration differences are measured rather than hidden.

Explore settings one variable at a time on a deterministic stratified sample:

```sh
python3 tools/sweep-eval.py /external/public/public.jsonl --per-tag 3 --out docs/eval-results/sweep
HYPE_FLOW_MODEL_TEST=1 python3 tools/sweep-eval.py /external/public/public.jsonl --per-tag 1 --sanity --out docs/eval-results/sanity
```

The sweep records worker/model fingerprints, no-vocabulary keyword insertions,
and process-tree RSS/PSS sampled every 100 ms. These sampled peaks cover the
whole evaluator lifetime, including loading and scoring, and may miss brief
transients. Sampling has timing overhead. Exploratory small-sample results do
not choose production defaults. Ground-truth boosting sanity requires lower
aggregate raw Whisper error counts, or preserves an already perfect baseline;
it exits nonzero if this fails. Reference-as-prompt case summaries are deleted
after aggregation to avoid storing full reference word lists as artifacts.

Model tests explicitly opt in through `HYPE_FLOW_MODEL_TEST=1`. They assert
trailing silence, trailing click and leading silence preserve exact raw text;
two copies separated by interior silence preserve both transcripts; pure
silence produces no text. Supply `golden_text` per case or `--golden` to enforce
case/punctuation-normalized word-LCS ≥1.0 for clips under 20 seconds and ≥0.95 otherwise. Optional `keywords`
and `checkpoints` fields assert token-boundary phrase presence and checkpoint order.
Clips up to 300 seconds retain baseline/golden/checkpoint checks; only augmented
variants exceeding the capture cap are skipped. `--report /external/report.json`
saves private raw mismatch details (0600); never commit that report. These tests use real inference
and may expose acoustic or chunk-boundary changes that model-free CI cannot.

Early local artifacts include `desktop/smoke.json` (one release-build nested-niri
replay sample), `invariance/summary.json` (word-preserving exact-format failures),
and the final 15-second-config JFK/quiet summaries with worker and model hashes.
The earlier direct-worker comparison changes several variables together and
serves as smoke evidence.

[Public corpus details](public/corpus.json) record the pinned sample. The
[Unified public baseline](public/unified.json) includes micro and macro WER,
per-set breakdowns, confidence counts, latency and worker RSS. The public
manifest deliberately retains unpadded short clips, including clips that are
shorter than the desktop capture minimum. This differs from Sayso's protocol.
The [Low Memory public result](public/low-memory/full.json) uses the same 1,400 clips
and manifest hash. Its run overlapped other evaluations. Personal speech was
outside this pass.

The [initial Parakeet 110M CTC result](public/ctc/full.json) scored 8.08%
Whisper WER at 276 ms p95 and 477 MB peak worker RSS on the same clips.
[CTC with original pauses preserved](public/ctc/pauses.json) scored
8.03% WER at 260 ms p95 and 477 MB peak worker RSS. The 110M TDT profile
scored 7.48% WER at 498 ms p95 and 482 MB peak worker RSS. These are separate
runs on the same host. A paired 70-clip check with three repeats had one
additional word error and a 12 ms higher p95 for pause preservation. Decoder
latency includes cleanup and excludes clipboard delivery.

The [CTC preroll result](public/ctc/preroll.json) uses the same
manifest and worker. It scored 8.06% Whisper WER at 256 ms p95 and 474 MB peak
worker RSS after normalizing digital leading silence.

With CTC installed, reproduce its configuration from the pinned Unified baseline:

```sh
hype-flow-evaluate /external/public/public.jsonl --config docs/eval.toml \
  --set 'model_variant="parakeet-ctc"' --set 'feature_dim=80' --summary /external/ctc.json
```

The [35-configuration public sweep](public/sweep.json) uses 21 speech clips,
with ten keyword-free controls. The [Low Memory sample](public/low-memory/sample.json)
uses the same clips. The [reference-hotword sanity check](public/hotword-sanity.json)
reports aggregate errors across six clips with at most 30 reference terms.
Sample size and desktop load limit runtime and default-setting conclusions.

The noise and silence sample uses one public clip per set with sweep seed 20261015. Each clip produces five variants. The results cover [baseline](public/augmented/baseline.json),
[gain](public/augmented/gain.json), [denoiser](public/augmented/denoise.json),
[VAD off](public/augmented/vad-off/baseline.json), and
[VAD off with denoiser](public/augmented/vad-off/denoise.json). The same source
clips provide the [speed and codec variants](public/media.json).
[Long-form scores](public/long/boundary.json) and [checkpoint results](public/long/checkpoints.json)
cover three recordings stitched from 40 individually verified LibriSpeech clean clips.

Measure capture end to paste with PCM replay in an isolated nested niri session.
Use an assets directory with Unified and its worker installed.

```sh
python3 tests/benchmark_desktop.py --assets /staged/share/hype-flow \
  --manifest /external/replay.jsonl --pace --repetitions 3 --output /external/desktop
```

The driver replays the first manifest row at 16 kHz and checks the pasted text
against the reference. It samples memory before the first recording and after
paste. The [short replay](desktop/paced-short.json)
uses an 11-second public clip and records three samples after warm-up. The
[long replay](desktop/paced-long.json) uses a 34.5-second clip and records two
samples after warm-up with clipboard tracing. Its results include nested
compositor and clipboard delays.

The default model check enforces exact formatting. `--word-invariance` gates
word preservation while recording exact formatting differences separately.
The [latest word-invariance report](invariance/final-word.json) retains the
four exact-format failures and removes transcript text from the published data.
