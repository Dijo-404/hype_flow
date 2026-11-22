# Hype Flow: Rust and GTK4 migration

Status: both phases implemented, verified and installed. Evidence and measured limits are recorded in `docs/migration-results.md`.
Date: 2026-11-21, Asia/Kolkata.

## Intent

Replace the Python application with a Rust application using GTK4. Preserve
the current offline dictation workflow, settings, model assets, and desktop
integration. The owner's stated goals are better memory management and speed.

The owner selected Rust and GTK4 and requested that the C worker also be
migrated, only after the main application migration is complete and tested.
The implementation details below remain the builder's recommendation.

## Findings and baseline

- The current application uses Python, PyGObject, GTK3, and GTK3 layer-shell.
- Speech inference already runs in a persistent, separate C process using
  sherpa-onnx 1.13.8 and the Parakeet Unified English INT8 model. Replacing
  Python alone does not replace ONNX inference or its model allocations.
- The interface has five pages: General, Speech model, Vocabulary, Text
  cleanup, and Shortcuts. Preserve these pages and their existing controls,
  dark appearance, compact layout, and keyboard behavior.
- `make check` on the existing source ran 76 tests: 73 passed and three
  desktop tests were skipped because they require an isolated desktop session.
  Python bytecode compilation also passed.
- One observation of the installed, background Python application with no
  recognizer process running measured 143456 KiB RSS and 99152 KiB PSS.
  This is an unloaded-model reference observation, not a controlled benchmark.
  The installed application and the source checkout have not been verified
  byte-for-byte during this audit.
- GTK4 4.24.1 and libadwaita 1.10.0 are installed. `cargo` and `rustc` are
  absent from PATH. The GTK4 layer-shell library was not found through
  pkg-config or under `/usr/lib`.
- The repository has no commits. Existing application files are untracked.
  Do not stage or overwrite the owner's entire tree to create a baseline.

## Approaches

1. **Selected sequence: Rust/GTK4 application, then Rust speech worker.**
   First port every Python runtime responsibility and the first-use setup
   flow to Rust, preserving the C worker until the main migration passes its
   verification gates. Then port the application-owned C worker to Rust as
   a separate, tested change. Both phases preserve the worker protocol,
   model, separate process, and network isolation. Worker memory and decoding
   speed will generally remain governed by sherpa-onnx and ONNX Runtime.
2. **Rust/GTK4 application with in-process inference through FFI.** This
   removes the pipe boundary but puts model failures into the UI process,
   changes cancellation and unload semantics, and loses the worker's separate
   network namespace. It offers no established model-memory improvement.
   Do not choose this without an explicit requirement and separate evidence.

The owner requested the staged sequence in approach 1. A permanent
Python/Rust hybrid does not fulfill the requested Python migration.

## Compatibility contract

Preserve the following externally visible behavior:

- The `hype-flow` executable, desktop entry `dev.hype.Flow.desktop`, icon,
  application ID `dev.hype.Flow`, and the existing GApplication object path
  `/dev/hype/Flow`.
- Private flags `--background`, `--toggle`, and `--cancel`; D-Bus actions
  `toggle`, `cancel`, `show`, `copy-last`, `copy-raw`, and `quit`.
- Stateful `status` action with exact GVariant signature `(ssddb)` and the
  existing state names, message, level, duration, and visibility semantics.
- Notch discovery at `dev.hype_niri.Notch`, action path
  `/dev/hype_niri/Notch`, and its `dictation` availability handshake. Send
  status and microphone levels, never transcript text, over this interface.
- Existing XDG config, data, and runtime directory resolution. Continue
  loading the current flat TOML settings, defaults, validation, legacy-default
  path migration, and custom model paths. Do not introduce a new config schema.
  Reject unknown settings as today. Write settings atomically and retain
  `config.toml.previous` when the model or server path changes.
- Previously downloaded model/runtime files, tokenizer, VAD and noise model
  assets, and `.previous` rollback copies. Do not redownload valid assets or
  delete old models during migration.
- The five-minute recording limit, mono 16 kHz signed PCM16 input, readiness
  cue after the first microphone frame, digital-silence handling, pause-aware
  segmentation, and every-sample preservation.
- Optional 30-second microphone hold and bounded half-second preroll, media
  pause/resume for exactly the players previously playing, cues, and model
  idle unload/reload using existing settings.
- Vocabulary boosting, capitalization, literal corrections, filler filters,
  protected spans, optional repetition removal, and fuzzy vocabulary matching,
  including current limits and precedence. Rust Unicode and regex behavior
  must be checked against Python outputs rather than assumed equivalent.
- Original and cleaned transcript recovery, including empty filtered output,
  delivery failures, private runtime permissions, and deletion at shutdown.
- Trusted transcript-hook execution, owner/permission checks, 30-second
  timeout, and fallback to cleaned text with a visible failure note.
- Focus restoration and confirmation before paste, copy-only fallback when
  the target closes, terminal-specific paste keys, no Enter in terminals,
  trailing-space preference, sensitive clipboard writes, and conditional
  restoration after 0.8 seconds. Clipboard contents changed by another app
  must not be overwritten by the restore step.
- Non-focusing overlay with click-to-finish and right-click-to-cancel, notch
  handover, reduced-motion preference, responsive settings, dirty-state save
  behavior, Ctrl+W to hide, and Ctrl+Q to quit.
- Explicit, validated, backed-up, idempotent niri shortcut installation with
  included-config conflict detection. Installing the app itself must not
  silently rewrite compositor configuration.

## Proposed implementation

Use one Cargo package with a library for testable core behavior and the
`hype-flow` application binary. Keep the structure close to the existing
modules. GTK objects and application state transitions belong to the GLib
main thread. Recording, inference, downloads, hooks, and slow subprocess work
run off that thread. Send results back with a recording-generation identity;
ignore stale completions so cancelled work cannot paste later.

Use the maintained `gtk4` Rust bindings and GTK4 layer-shell bindings. Keep
the existing custom interface; libadwaita is not required for this migration.
Port GTK3 drawing and input handling to GTK4 drawing callbacks, gestures, and
event controllers. Use weak GTK references and remove timers or callbacks
when their owners stop, to avoid reference cycles and idle animation work.

Use Rust-owned bounded audio storage. The maximum PCM payload is 9600000
bytes (300 seconds at 16000 Hz, two bytes per sample); preroll must fit inside
this limit. Slice existing storage for segmentation instead of copying the
entire recording for each chunk. Bound microphone-level notifications so a
slow UI cannot accumulate an unlimited queue.

Keep the native worker as a child process with anonymous pipes. Preserve
little-endian length prefixes, sample counts, the ready handshake, the 1 MiB
response limit, full writes, startup/inference deadlines, and bubblewrap
availability fallback with visible diagnostic logging. Child ownership must
cover cancellation, partial startup, idle unload, application shutdown, and
forced termination after a bounded grace period. A blocked read or write must
not prevent cancelling and reaping the child.

In phase 1, port setup into Rust, preserving pinned URLs, SHA-256 hashes, resumable
downloads, bounded tokenizer parsing, token-set verification, extraction
safety, expected model-size checks, staged runtime compilation, version
markers, and rollback copies. Do not upgrade the speech model or inference
runtime as part of this migration. Include the C source as a shipped resource
for the existing one-time runtime compilation flow during phase 1. Phase 2
replaces that compilation step with installation of the release-built Rust
worker while retaining the existing native inference libraries.

Use direct, maintained dependencies where the standard library is insufficient:
GTK/GLib/GIO, TOML/Serde, HTTP/TLS, SHA-256, archive extraction, and matching
support. Select compatible versions and commit Cargo.lock during
implementation. No async runtime, plugin system, framework, or extra settings
are required by this design.

## Files and responsibilities

| Current location | Migration responsibility |
| --- | --- |
| `src/dictation/app.py`, `controller.py` | Rust entry point, GApplication actions, state machine, lifecycle and cancellation |
| `src/dictation/ui.py`, `settings.css`, `logo.svg` | GTK4 settings, overlay, gestures, styling and embedded assets |
| `src/dictation/config.py` | TOML compatibility, validation, permissions and atomic saves |
| `src/dictation/audio.py`, `segments.py` | PipeWire capture, bounded audio/preroll, levels and borrowed chunk slices |
| `src/dictation/backend.py`, `native/recognizer.c` | Phase 1: Rust worker owner with the C worker; phase 2: Rust worker executable with the same inference protocol |
| `src/dictation/cleanup.py`, `vocabulary.py` | Equivalent cleanup, matching, input limits and tokenizer handling |
| `src/dictation/output.py`, `hook.py`, `feedback.py` | Safe clipboard delivery, hook execution, media and sounds |
| `src/dictation/setup.py`, `shortcuts.py` | Verified model setup and reversible niri integration |
| `tools/install.py`, `launch.py`, `evaluate.py` | Python-free install/development commands and equivalent Rust evaluation tool |
| `tests/`, `Makefile`, `pyproject.toml`, `.gitignore` | Parity fixtures, Rust tests/build checks, packaging transition and build ignores |
| `README.md`, `docs/config.toml`, `docs/third_party.md` | Updated build requirements, unchanged settings, runtime/provenance documentation |

## Required phase order

### Phase 1: application migration and verification

Complete the Python-to-Rust and GTK3-to-GTK4 migration using the existing C
worker. Finish the application acceptance checks below, including desktop
integration, cancellation, recovery, release build, and performance comparison.
Record the actual results and any remaining failures or skipped checks.

**Gate:** Do not start implementing the worker rewrite while required main
migration checks remain failing, skipped, or unverified. Preparation and
planning may identify its interfaces, but worker implementation starts only
after the main migration is complete and tested. Missing desktop or hardware
verification must be reported rather than treated as a passing gate.

### Phase 2: C-to-Rust worker migration

Replace the application-owned `src/dictation/native/recognizer.c` with a
separate Rust worker executable. This phase is required work, not an optional
future improvement. Keep bubblewrap network isolation, anonymous pipes, the
existing command-line arguments, and the length-prefixed protocol. Preserve
the installed `server` path so existing settings continue to work.

The Rust worker continues calling the pinned sherpa-onnx C API and ONNX
Runtime. Rewriting those upstream libraries or the model is outside scope.
Use bindings derived from the pinned header rather than guessed ABI layouts.
Keep unsafe operations at the FFI boundary and use owned handles with
deterministic cleanup for the recognizer, streams, results, VAD, denoiser,
speech segments, and denoised buffers. Keep strings and sample buffers alive
for as long as the native API requires them.

Preserve PCM conversion, the 300-second input limit, greedy and boosted
decoding, BPE vocabulary scores, VAD windowing and padding, overlapping-span
handling, denoising, and the fallback to decoding all audio when VAD finds no
speech. Preserve error exits for malformed or truncated input and clean EOF
handling. Flush each framed response and keep diagnostics on stderr.

Before worker cutover:

- Compare C and Rust workers on the same prerecorded corpus and settings:
  greedy decoding, vocabulary boosting, VAD, denoising, and combined options.
  Verify equivalent raw transcripts, valid response framing, and unchanged
  downstream cleaned transcripts and word-error rates.
- Test partial reads, truncated headers/payloads, zero and oversized sample
  counts, invalid arguments, repeated requests, clean EOF, startup failure,
  cancellation, shutdown, and idle unload/reload through the Rust application.
- Measure both workers' startup, decode time, and process-tree memory using
  the same repeated scenarios as phase 1. Check resource cleanup on errors
  and repeated requests; do not infer improvements from the language change.
- Test staged installation and rollback, native-library loading, and the
  worker version marker. Ship the release-built Rust worker and retain a
  known-working C worker for rollback. The normal installation flow must no
  longer require compiling the application-owned C source.

**Gate:** Replace the installed C worker only after the Rust worker passes
these checks and the application integration suite remains green. Keep the
tested phase-1 application and C worker available if verification fails.

## Rollout and rollback

Build the Rust implementation alongside the existing source until parity
checks pass. Keep the Python implementation available as the migration oracle
and rollback source; removal is a later, explicit contraction, not a
prerequisite for using the Rust app. The installed Rust app must have no
Python runtime dependency, including setup, installation, and evaluation.

Build and stage a release artifact before touching the installed launcher.
Test using isolated XDG roots and a separate D-Bus session, with prerecorded
or synthetic audio where possible. Do not run both implementations under the
same application ID in the user's session.

After the parity and performance checks, retain the existing installed Python
launcher/application and desktop entry as a rollback installation. Stop the
old instance through its normal quit action before replacing the launcher.
Use atomic replacement of the staged Rust executable. Verify the new process,
actions, and assets before declaring cutover successful. On failure, stop the
new process and restore the old launcher/desktop entry. Do not remove model
files, config, or niri bindings during rollback. Preserve changes saved after
cutover; restore a config backup only when compatibility actually requires it.

## Phase-1 acceptance evidence

1. `cargo fmt --check`, warning-clean linting, unit/integration tests, and a
   release build succeed. Existing Python tests remain runnable during the
   compatibility window. Port behavioral assertions, not Python-specific mocks.
2. Differential fixtures cover every cleanup/vocabulary option, Unicode
   boundaries/case behavior, config validation and roundtrip, PCM chunking,
   malformed worker responses, tokenizer failures, and word-error-rate scoring.
3. Lifecycle tests cover first start, concurrent worker loading, microphone
   failure, maximum duration, cancellation during load/decode, stale callbacks,
   duplicate stop requests, idle unload/reload, hooks, and shutdown cleanup.
4. Isolated desktop tests cover all five pages, compact layout, preferences,
   model activation, launcher flags/actions/status signature, notch handover,
   no-focus overlay, transcript recovery, and focus/clipboard/terminal behavior.
   Live microphone and typing tests require a controlled target and must not
   paste into whichever user application happens to be focused.
5. A prerecorded speech corpus has identical raw recognition results when
   using the same worker/model/options, and equivalent cleaned results and
   word-error rates. Test greedy and vocabulary-boosted modes, VAD and noise
   suppression. A tiny smoke corpus is regression evidence, not a broad
   accuracy claim.
6. Benchmark release Rust and Python under the same desktop, model, threads,
   options, and audio. Record cold/warm startup, shortcut-to-first-frame,
   decode time, and end-to-end delivery time. Measure whole-process-tree PSS
   and RSS separately with the model unloaded, loaded, during long recording,
   and after idle unload. Repeat each scenario at least five times and report
   medians and ranges. Do not change defaults to manufacture a speed gain.
7. Memory is bounded over repeated record/cancel/unload cycles. No child,
   microphone, transcript, vocabulary file, GTK callback, or model allocation
   survives its intended lifetime. Account for shared model/library pages
   when reporting memory totals.
8. Report actual deltas. Lower unloaded application PSS is the intended
   outcome; decoding and interaction timing must show no reproducible
   regression outside measurement variation before replacing the installation.
   No percentage saving is promised in advance. If this gate fails, keep the
   Python installation active and report the measurements before cutover.
9. Verify installation and rollback with temporary XDG roots, then smoke-test
   the installed Rust app without Python in its runtime path.

## Scope limits and remaining decisions

No model replacement, GPU support, cloud processing, new features, visual
redesign, or rewrite of upstream inference libraries is included. The
application-owned C worker rewrite is included as phase 2, after phase 1
passes its tests. Keep the existing inference memory unload setting: Rust
ownership cannot remove memory required by a loaded model.

The owner specified the two-phase order. Dependency versions, exact Rust
module boundaries, and test helpers are left to the implementer. Installing
missing build dependencies occurs after design review; each installation
cutover occurs only after its phase's verification gates pass.

## Upstream references

- [GTK's Rust bindings](https://www.gtk.org/docs/language-bindings/rust/)
- [gtk4-rs API documentation](https://gtk-rs.org/gtk4-rs/stable/latest/docs/gtk4/)
- [GTK4 layer-shell upstream](https://github.com/wmww/gtk4-layer-shell)
- [GTK3 to GTK4 migration guide](https://docs.gtk.org/gtk4/migrating-3to4.html)

The local source and tests define Hype Flow's compatibility contract; these
references describe the target toolkit, not evidence of performance gains.
