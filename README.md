# Hype Flow

Native offline English dictation for niri. Open **Hype Flow** from your launcher. Press **Alt + Space**, speak, then press again to paste into the original window. **Super + Ctrl + Escape** cancels.

The default model is **Parakeet 110M CTC English INT8**. Its installed model files total **131.7 MB**. It supports English with punctuation and capitalization. At login, Hype Flow starts without loading the speech model. The CPU worker loads in parallel with the first recording and unloads after 10 idle minutes by default (Speech model → Memory). It exchanges audio and results over anonymous pipes.

## Desktop setup

The Hype Niri `hype-flow` installer option sets up the model, shortcuts, and background startup. For manual setup in the app, open **Speech model** and choose **Install selected model**. Setup verifies the selected archive and pinned native runtime before activating the model. To switch models, choose **110M TDT** or **Unified**, save the choice, and install it. Existing installations with a saved model continue using it until you install a different one.

CTC worker revision 8 preserves the original pauses within each decode piece. After updating Hype Flow, refresh an older CTC worker through **Speech model** by installing the CTC profile again. Setup reuses a verified cached archive when available.

With silence cutting enabled, Parakeet CTC uses a fixed 32 ms digital lead-in before decoding. This keeps added all-zero audio from shifting the decode boundaries. Dictation duration and word timestamps still refer to the original recording.

Choose **Configure niri** under **Shortcuts** to add bindings and autostart manually. The app validates the resulting configuration and saves a backup. Existing display settings at `Super + Shift + D` remain available.

Click the current **Dictate** or **Cancel** shortcut under **Shortcuts**, then press the key combination you want, such as `Alt+F9`. Press Escape or click away to cancel capture. Choose **Save changes** to apply the new shortcut. Existing preferences keep the original shortcuts until edited. Changes validate the complete niri configuration, reject conflicts, and back up both the configuration and the managed bindings.

For Alt-based shortcuts in VS Code, set `"window.customMenuBarAltFocus": false` in VS Code's settings. Otherwise, releasing Alt can focus the menu bar and leave Chat without keyboard focus, so dictation starts but its text does not reach the Chat input.

Settings use a macOS-style dark layout: a floating sidebar with window controls, grouped rows, and subtle page transitions. The window adapts to narrow layouts. **Save changes** becomes available after an edit and applies supported preferences. Closing the window with its close button or **Ctrl+W** leaves dictation available in the background; **Ctrl+Q** quits Hype Flow.

With `hype_niri`'s `feature/niri-notch` integration, the waveform capsule appears inside the notch. Otherwise a native fallback capsule appears near the bottom of the screen. Click the capsule to finish recording or right-click to cancel. The notch drawer also provides dictation, cancel, copy-last, and settings controls. Recording surfaces do not take keyboard focus.

Start speaking at the start sound, which plays once the microphone delivers audio (PipeWire takes about 200 ms). Each dictation stops at five minutes. Cancelling inference releases the worker; the next dictation reloads the model. A recording of pure digital silence reports a muted microphone instead of "No speech detected".

**General** also offers: a trailing space after pasted text; Enter after pasting, which sends chat messages and is never sent to terminals; a microphone picker; start and stop sounds; pausing media players that are playing, then resuming exactly those; and **Keep microphone ready**, off by default, which holds the microphone open for 30 seconds after a dictation so the next one starts instantly and includes the half second before the shortcut. The microphone indicator stays on while it is held.

## Installation from source

Requires Linux x86_64, a current stable Rust toolchain, GTK4, gtk4-layer-shell, PipeWire, wl-clipboard 2.3+, wtype, bubblewrap, and niri. The application and speech worker build with Cargo; first-use model setup does not compile C code. On Arch:

```sh
sudo pacman -S base-devel rust gtk4 gtk4-layer-shell pipewire wl-clipboard wtype bubblewrap
make install
~/.local/bin/hype-flow --setup
```

`make install` creates the desktop launcher and installs release binaries. Run `--setup` to download the configured model, validate and configure the Niri shortcuts, and enable background startup at login. Setup works from a terminal without a graphical session. It reuses installed models and saved shortcuts. The developer evaluator uses Python for the pinned Whisper English scoring normalizer. An existing launcher and desktop entry are retained under `$XDG_DATA_HOME/hype-flow/rollback`.

## Project layout

```text
src/*.rs               Rust application, GTK4 UI and speech pipeline
src/bin/evaluate.rs    Rust word-error-rate evaluation tool
worker/                Rust inference worker and pinned generated API bindings
assets/                Embedded stylesheet, logo and common-word list
tools/                 Installation, evaluation and desktop verification tools
tests/                 Rust tests, regression fixtures and test drivers
docs/                  Configuration, provenance and migration evidence
Cargo.toml             Rust package metadata and desktop entry point
Cargo.lock             Pinned Rust dependency versions
```

For development, run `cargo build --workspace`, then `cargo run --bin hype-flow`. If GTK4 layer-shell is installed under `~/.local`, include `~/.local/lib/pkgconfig` in `PKG_CONFIG_PATH` when invoking Cargo directly. Make sets this automatically.

`make release` builds the desktop app and the worker used by first-use model setup. The [migration results](docs/migration-results.md) record the Python-to-Rust and C-to-Rust comparisons.

## Accuracy and limits

Parakeet 110M CTC prioritizes fast English dictation on CPU. Unified remains available for accuracy-focused use. See [provenance](docs/third_party.md) and [benchmark results](docs/eval-results/README.md).

Long recordings use pause-aware chunks capped at 15 seconds by default. Completed pieces decode during capture, so stopping usually leaves only the final piece. **Cut silence before recognition** runs Silero VAD in the worker. It keeps up to 450 ms before speech and 200 ms after, and returns empty text if it detects no speech. The optional `soft_speech_retry` uses an energy gate to allow one retry on sustained audible input. Recordings under 300 ms are rejected. `backend.log` records how much audio VAD kept.

**Suppress background noise** (off by default) runs sherpa-onnx's GTCRN speech-enhancement model before voice detection. It adds about 0.5 seconds per dictation. In a test with white noise down to 3 dB SNR, Parakeet already transcribed without errors with and without it, so enable it for loud rooms and compare with `hype-flow-evaluate` on your own recordings. Both small models (Silero VAD 644 KB, GTCRN 536 KB) install with the speech runtime and are checksum-verified.

This runtime uses INT8 CPU inference. **Vocabulary** applies preferred capitalization after recognition. Unified supports optional native decoder boosting from 0.5 to 3.0. Boosting can slow decoding and insert unwanted words. Compare it on your own recordings using `hype-flow-evaluate --config docs/eval.toml --boosting on` and `--boosting off` with Unified installed.

**Near-miss correction** is opt-in. It joins split names such as “Charge B” and uses bounded spelling and phonetic checks for unique vocabulary matches. Common words, short spellings, ambiguous matches, and paths are protected. Explicit correction rules take priority. The optional confidence gate holds back guessed corrections unless the matching words have low decoder confidence.

**Text cleanup** applies your configured literal corrections and preferred vocabulary capitalization. Add one rule per line, for example `open ai => OpenAI`. Matching ignores case and uses whole words or phrases; it does not guess spelling or rewrite grammar. By default, filtering removes `um, uh, erm, umm, uhh`. Edit that list to remove other unwanted words or phrases. URLs, email addresses, quoted text, inline code, and unlisted uppercase acronyms are preserved. Adjacent repeated-word removal is optional and off by default; meaningful words such as “like” and “you know” remain unless you list them.

**Copy original** under **General** recovers the latest recognition result before text cleanup. If filtering removes everything, no paste occurs and the original remains recoverable. Disabling corrections or filtering takes effect without reloading the model.

Common preferences are available in the desktop window. Advanced paths, CPU threads, and microphone target are documented in [docs/config.toml](docs/config.toml). Model files live under `$XDG_DATA_HOME/hype-flow`, defaulting to `~/.local/share/hype-flow`. Previous default Whisper paths migrate to Parakeet; custom paths stay intact until a new model is explicitly installed. Applying a new model saves previous settings to `config.toml.previous` and retains other preferences. Old downloaded models are not deleted.

For custom post-processing, an executable you own at `~/.config/hype-flow/transcript-hook`, not writable by others, receives each cleaned transcript on standard input; its standard output is pasted instead. It has 30 seconds. If it fails, times out or is untrusted, the cleaned transcript is pasted and the status line says why.

## Paste and privacy

Text is copied with `wl-copy` and pasted with `wtype`. VS Code, Cursor and VSCodium use `Shift+Insert`, following OpenWhispr's editor-specific Linux route; both the clipboard and primary selection receive the transcript for these apps. About 30 known terminal applications use `Ctrl+Shift+V`; other applications use `Ctrl+V`. Embedded terminals and custom shortcuts may require automatic paste disabled. If focus changes during inference, Hype Flow returns focus to the window where dictation started, confirms it, and pastes there. If that window has closed, the text is copied without typing. Focus can still change between the final check and keystroke delivery.

The optional `modifier_release_guard` setting polls already-readable Linux keyboard devices before pasting. It waits at least 50 ms and cancels automatic paste after 1.5 seconds if modifiers remain held. Without keyboard access, it keeps the existing 180 ms delay. No input-device permissions are changed.

Fuzzy vocabulary matching is disabled by default. When enabled, it protects common words, short spellings, ambiguous matches, and paths. Exact vocabulary capitalization and configured corrections remain available. Native hotword output is capped at 100 entries after case variants. Boosted output that echoes the vocabulary list is discarded; punctuation-only output, `<unk>`, and common noise tags are filtered. The original recognition remains recoverable through **Copy original**. Optional repeated-word removal preserves “had had”, “that that”, “very very”, “no no”, “yes yes”, “ha ha”, and “so so”.

After an automatic paste, Hype Flow restores the previous clipboard 0.8 seconds later, unless something new was copied in the meantime; an empty clipboard is cleared again. For the editor-specific `Shift+Insert` route, the previous primary selection is restored independently unless it changed. When text is only copied, the clipboard and the primary selection (`Shift+Insert`) keep the transcript. Hype Flow marks its clipboard writes sensitive, but clipboard managers may still save them. The latest original and cleaned transcripts stay in mode-0600 runtime files until application exit or replacement. A nonempty vocabulary list also occupies a private runtime file while its speech worker is loaded. By default, audio stays in memory and travels to the native worker through pipes. Recovery and history persist audio only when explicitly enabled. The worker runs inside a bubblewrap network namespace with only loopback, so it cannot reach any network; `backend.log` records whether the sandbox was active. **Copy transcript** recovers text after a paste failure. There is no transcript database.

The notch receives state and microphone levels over session D-Bus, never transcript text. Its existing weather and web-search features remain independent of offline dictation. Runtime diagnostics are stored at `$XDG_RUNTIME_DIR/hype-flow/backend.log`.

## Developer validation

```sh
make check
make check-python
```

Run `make check-desktop` only inside a controlled Wayland compositor and an isolated D-Bus session. It uses synthetic capture and its own paste target. The desktop driver requires Python/PyGObject only as development test tooling.

The evaluation harness uses the same persistent engine and chunking as live dictation, without recording or clipboard output. Supply mono 16 kHz PCM16 WAV files and a JSONL manifest with paths relative to it:

```json
{"audio":"english.wav","text":"Expected spoken words."}
```

```sh
hype-flow-evaluate recordings.jsonl --max-wer 0.05
```

The harness reports per-clip time, original recognition text, cleaned text, and both word error rates. It fails when cleaned word error rate exceeds the chosen threshold. Normalization ignores punctuation and case. The smoke corpus is only a regression check, not a broad accuracy benchmark.


Opt-in recording recovery and history are available in General settings. Recovery
keeps the latest 20 failed/interrupted recordings; history keeps the latest 20
successful recordings. Audio is stored under `$XDG_STATE_HOME/hype-flow` in
mode-0700 directories with mode-0600 WAV files. Interrupted raw spools are recovered
at startup when either storage option is enabled. **Retry last failed** and
**Retry recent recording** re-run recognition and copy the result for review;
they do not guess an old application's input target. **Delete saved recordings**
clears persisted audio. Turning retention off does not erase existing files.
With both storage options off, capture remains memory-only.

Optional text controls add spoken punctuation, whole-utterance **New line**,
**New paragraph** and **Scratch that**, bounded restart removal, and conservative
English number/money/time formatting. Scratch that retracts only its own utterance;
it never deletes text from another application. Capital handoff after filler
removal is separately selectable. URL, email, quoted and inline-code spans remain
protected. Original recognition is still available through **Copy original**.

Confidence filtering permits fuzzy vocabulary changes only when metadata marks
all candidate words as low-confidence. Missing or conflicting confidence keeps
the original spelling. Optional hallucination filtering rejects anomalous timing
and token probabilities while retaining the original transcript for review.
These gates default off pending calibration on representative speech.

**Shortcut behavior** selects toggle or hold. Hold uses niri native press/release
bindings; saving it validates compositor support before changing shortcuts. The
installed niri 26.04 build does not support those bindings, so it rejects hold
without changing the existing settings or bindings. No input-device permissions
are changed. Model changes keep the current model active until installation
finishes; low-memory activation disables unsupported native boosting and resets
model-local runtime experiments to CPU defaults. Vocabulary text corrections
and preferred spellings remain available.
