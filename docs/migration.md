# Hype Flow migration verification

The migration plans, Python application, C worker, and parity scripts are
available in Git at revision `7938c5a8e883f18499aded4b0884d8c2ca668d0f`.

## Phase 1: Rust/GTK4 application

The application migration passed its functional, build, source-review and
measurement checks before worker implementation began. The existing C worker
and sherpa-onnx 1.13.8 model/runtime were retained for this checkpoint.

### Verified behavior

- `make check`: 28 ordinary Rust tests pass, formatting passes, and
  `cargo clippy --all-targets -- -D warnings` passes. The two environment-gated
  Rust tests were also run explicitly and passed: GTK4 desktop and real
  PipeWire readiness, for 30 distinct Rust tests in total.
- The production executable passed controlled capture-to-paste tests under
  nested niri and a private D-Bus session. These cover actions/status `(ssddb)`,
  startup flags, all five settings pages, validation/save/compact layout,
  no-focus overlay, notch appearance/action removal/disappearance, cleanup,
  original recovery, clipboard restoration, cancellation, rapid pending-open
  callbacks, shutdown during delivery, and transcript deletion on exit.
- The real first-use setup action reused checksum-verified cached archives,
  activated the installed model, forced English/CPU, preserved paste and
  vocabulary preferences, and retained the prior config backup.
- Installation in temporary roots passed desktop launch with spaces and
  reserved characters in the prefix, private-library loading with a separate
  XDG data root, repeated installation, and restoration of the previous
  launcher and desktop entry. No compositor settings were changed.
- A real 300-second idle expiration was observed after 305 seconds. Loaded
  process-tree PSS fell from 1497709 KiB to 31314 KiB. The next recording
  started while its worker reloaded. This is one real timer event, not five
  independent idle-unload cycles.
- Real PipeWire produced the first frame in 370 ms in one readiness smoke
  check. Its 4096 captured PCM bytes were discarded and never persisted.
- Cleanup matched 28 fixed fixtures and 519 generated Python comparisons.
  The pinned 1024-piece BPE vocabulary export was byte-identical; 2493 extra
  score-format cases also matched. Native greedy, vocabulary boosting, VAD,
  denoising, and combined modes matched Python raw and cleaned transcripts
  with zero word errors across the 88-word smoke corpus.
- The Python baseline suite served as an oracle. All 76 tests,
  including its three desktop tests, passed under an isolated D-Bus/Wayland
  session and empty XDG configuration. An initial legacy desktop run inherited
  the owner's non-default preferences and failed two default-value assertions;
  isolating the test configuration resolved those harness failures without
  changing the Python implementation.
- Independent source review identified shutdown, pending-recorder, clipboard,
  idle-timer, and packaging defects; fixes and regression checks were completed.

### Paired measurements

Five measured repetitions per application/scenario followed an unmeasured
warm-up. Order alternated, host caches were left alone, the model and C worker
were identical, and capture used the same prerecorded audio. Values are
medians; full samples/ranges are retained in the development evidence.

| Metric                           |      Python |   Rust/GTK4 | Observed change |
| -------------------------------- | ----------: | ----------: | --------------: |
| Model-free process-tree PSS      |   40621 KiB |   28235 KiB |          -30.5% |
| Model-free process-tree RSS      |  100548 KiB |   61484 KiB |          -38.9% |
| Loaded process-tree PSS          | 1497274 KiB | 1443804 KiB |           -3.6% |
| Loaded startup to ready          |     4822 ms |     4795 ms |           -0.6% |
| Startup to setup prompt          |      361 ms |      381 ms |           +5.3% |
| Shortcut to first captured frame |      318 ms |      168 ms |          -47.0% |
| Capture end to pasted completion |     1315 ms |     1171 ms |          -11.0% |
| Decode/cleanup subphase          |      971 ms |      814 ms |          -16.2% |
| Delivery subphase                |      241 ms |      331 ms |          +37.4% |

Settings are created when first opened, reducing background memory. Pidfd
exit notifications avoid unnecessary waits after helper processes close their
pipes. The delivery subphase retains additional focus, clipboard, cancellation
and recovery checks and is slower in isolation. The acceptance comparison uses
the complete user interaction: shortcut readiness and capture-end-to-paste both
improved. Startup differences overlap the measured ranges. No safety check or
inference default was disabled to obtain these results.

These figures describe this machine and controlled setup, not guaranteed gains
for all hardware or speech. The model accounts for most loaded memory. A small
public smoke corpus does not measure accuracy for the owner's own voice, and
the latency benchmark uses synthetic capture rather than a physical microphone.

### Evidence files

Development evidence is under `.migration/`: `final-check.log`,
`desktop-integration.log`, five UI captures, `model-activation-inside.log`,
`installation.log`, `microphone.log`, `idle-unload.json`, `benchmarks.json`,
`benchmark-report.md`, and the core/runtime/services/final source-review reports.
The Rust executable was installed and started successfully with the existing
C worker. Saved configuration bytes were unchanged and the application reported
`idle / Ready`; `phase1-live.json` records the checkpoint. Normal shutdown of
the previous application cleared its session-only transcript files, as before.

The private baseline source snapshot and phase-1 release binaries remain there
for rollback and comparison; this directory is ignored by git.

## Phase 2: Rust speech worker

The application-owned wrapper is now Rust, installed only after the phase-1
checkpoint above. It remains a separate network-isolated process and loads the
same sherpa-onnx 1.13.8 and ONNX Runtime native libraries. Those upstream
libraries and the model were not rewritten or replaced.

### Verified behavior

- Final `make check` passes: 36 distinct ordinary Rust tests, formatting and
  warning-clean workspace Clippy. The explicitly run GTK4 and real PipeWire
  tests bring the total to 38; nested subprocess executions of a core test
  are not counted again. The preserved Python suite passed all 76 tests.
- Seven worker protocol tests cover CLI validation, PCM framing and bounds,
  partial requests, readiness/result flushing, EOF, malformed input, native
  failures, resource destruction, response limits, denoising fallback and
  overlapping VAD padding.
- Generated bindings require all 21 used native functions. Independent C/Rust
  checks matched 33 struct sizes/alignments and 155 field offsets. The pinned
  header and exact regeneration procedure are documented in
  [third-party.md](third-party.md); regeneration reproduced the file byte for byte.
- C and Rust transcripts were identical for all three clips in greedy,
  boosting, VAD, denoising and combined modes over five measured repetitions
  per mode. This establishes wrapper parity, not an accuracy guarantee for
  processed speech or the owner's voice.
- The production app passed desktop lifecycle, cancellation, stale-opener,
  delivery-shutdown, clipboard/recovery and no-focus overlay checks. The
  actual first-use setup action staged and initialized the Rust worker with
  a deliberately unusable C compiler on PATH. Settings and their backup were
  preserved. A truncated ELF is rejected before activation.
- Repeated temporary installation passed real desktop launching, unusual
  installation paths, private-library resolution, atomic inode replacement
  and rollback. Independent review findings were fixed and re-reviewed.
- One real five-minute idle expiration completed after 305.2 seconds. Loaded
  process-tree PSS fell from 1456969 KiB to 30644 KiB; the next recording
  reloaded the worker successfully. This measures one actual timer cycle.

### Worker measurements

Five startup pairs alternated C/Rust order. Each processing mode used a warm-up
and five alternating corpus pairs, with both workers resident for decode.
Values below are medians with minimum–maximum ranges in milliseconds. The
three-clip corpus total includes its silence clip.

| Measurement             |               C worker |            Rust worker |
| ----------------------- | ---------------------: | ---------------------: |
| Startup to ready        | 4518.9 (4275.4–4791.9) | 4554.6 (4244.8–4700.0) |
| Greedy corpus decode    | 3373.5 (3164.9–3547.6) | 3297.4 (3211.9–3660.3) |
| Boosting corpus decode  | 4032.4 (3779.4–4208.9) | 4000.7 (3611.7–4088.4) |
| VAD corpus decode       | 4090.8 (3799.4–4411.3) | 3862.2 (3723.2–4129.3) |
| Denoising corpus decode | 5675.6 (5325.4–6042.0) | 5644.5 (5301.8–5933.7) |
| Combined corpus decode  | 5569.7 (5207.6–6212.8) | 5726.5 (5113.0–6116.9) |

Ranges overlap throughout. These observations establish no reproducible worker
timing regression outside the measured variation, and no reliable inference
speed gain. Most work still occurs in the same native inference libraries.
Startup PSS was 1417252 KiB for C (1370987–1451153) and 1440172 KiB for Rust
(1401096–1475095), with overlapping ranges. Decode PSS samples share native
pages between resident workers and must not be treated as isolated memory savings.
The earlier fixed-order run is retained as supplemental evidence, not a speed claim.

Five fresh-process measurements of the complete Rust app and Rust worker gave
capture-end-to-paste median 1134.7 ms (1067.4–1175.1), shortcut-to-frame 150.1 ms
(147.8–167.4), and model-free PSS 28157 KiB (28124–28369). This later run is
descriptive; it is not a new paired comparison against Python. The paired
phase-1 comparison above isolates the application migration.

### Installed state and rollback

The installed Rust app reported `idle / Ready` with Rust worker marker `4`.
Its worker SHA-256 matched the measured release binary, and its backend log
confirmed active bubblewrap network isolation. Saved configuration bytes and
any existing session recovery files were preserved across this cutover.
The shipped application, evaluator and recognizer are native executables;
Python remains only as the retained development oracle and rollback source.

On this installation, the phase-1 backup directory is
`~/.local/share/hype-flow/rollback/phase1-rust/`. It contains:

- `0-hype-flow`, `1-hype-flow-evaluate`: verified phase-1 Rust executables.
- `2-recognizer`, `3-recognizer.version`: the C worker and its version marker.
- `4-dev.hype.Flow.desktop`, `5-libgtk4-layer-shell.so.0`: prior desktop entry
  and private GTK layer-shell library.

To return to phase 1, quit the app through its `quit` action, restore both the
phase-1 app and C worker with their matching marker, then restart in background.
Restore the evaluator, desktop entry and private library to their corresponding
installed paths if reverting the complete installation. Keep the runtime's
`lib/` directory with the worker. Do not pair the phase-2 app's version check
with the old marker. The original Python launcher is separately retained at
`~/.local/share/hype-flow/rollback/hype-flow`; its source and installed modules
remain available. Model files and preferences need no rollback.

Development evidence includes `.migration/worker-completion-check.log`,
`worker-final-release.log`, `worker-final-review.md`, `worker-desktop.log`,
`worker-installation.log`, `worker-model-activation.log`,
`worker-balanced/worker-comparison.json`, `phase2-benchmark/`,
`worker-idle-unload.json`, `abi-layout.json`, `bindings-regenerate.log`, and
`phase2-live.json`. The private source snapshot and phase-1 binaries remain
available alongside these ignored development artifacts.

## Verification before committing

Fresh checks on November 22, 2026 passed the release build, formatting,
strict workspace Clippy, 36 ordinary Rust tests, the explicit real PipeWire
and GTK4 tests, all 76 Python tests, installation/rollback and production
model activation. The newly tracked `tests/fixtures/niri.kdl` replaces a
compositor fixture that previously existed only in ignored local evidence.
The fresh release worker still matches the SHA-256 in the native parity report.

One desktop repeat, launched concurrently with another nested compositor
test, timed out waiting for pasted text. A subsequent sequential diagnostic run
and the full serial desktop command passed. The cause of the intermittent
timeout remains unconfirmed. Treat the GUI
driver as requiring a controlled compositor and retain this limitation when
interpreting its results. Performance and the five-minute timer were not
remeasured solely for this commit; their existing evidence remains above.
