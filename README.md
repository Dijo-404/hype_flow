# Hype Flow

Native offline English dictation for niri. Open **Hype Flow** from your launcher. Press **Super + Ctrl + Space**, speak, then press again to paste into the original window. **Super + Ctrl + Escape** cancels.

The default model is **Parakeet Unified English INT8**: **663 MB of installed model files**, below 1 GB. This limit describes model files, not total runtime RAM. The model supports English with punctuation and capitalization. Its CPU runtime stays loaded between recordings, unloads after 10 idle minutes by default (Speech model → Memory) to free about 1.4 GB, and reloads in parallel with the next recording. It communicates through anonymous pipes, with no network listener.

## Desktop setup

On first use, open **Speech model** and choose **Download model · 663 MB**. Setup verifies SHA-256 checksums, installs the 501 MB compressed model archive and pinned native runtime, and stages the shipped Rust worker. Normal dictation never downloads anything or contacts external services.

Choose **Configure niri** under **Shortcuts** to add bindings and autostart. The app validates the resulting configuration and saves a backup. The `hype_niri` branch includes these bindings directly. Existing display settings at `Super + Shift + D` remain available.

Click the current **Dictate** or **Cancel** shortcut under **Shortcuts**, then press the key combination you want, such as `Alt+Space`. Press Escape or click away to cancel capture. Choose **Save changes** to apply the new shortcut. Existing preferences keep the original shortcuts until edited. Changes validate the complete niri configuration, reject conflicts, and back up both the configuration and the managed bindings.

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
```

The installer creates the desktop launcher and installs release binaries without editing compositor settings. The installed application, first-use model setup, and evaluator do not require Python. An existing launcher and desktop entry are retained under `$XDG_DATA_HOME/hype-flow/rollback`; model and configuration files stay in place.

## Project layout

```text
src/*.rs               Rust application, GTK4 UI and speech pipeline
src/bin/evaluate.rs    Rust word-error-rate evaluation tool
src/dictation/         Previous Python application, kept for parity and rollback
worker/                Rust inference worker and pinned generated API bindings
src/dictation/native/  Previous C worker, kept as a compatibility reference
assets/                Embedded stylesheet and logo
tools/                 Installation and controlled desktop verification
tests/                 Rust tests, differential fixtures and Python baseline
docs/                  Configuration, provenance and migration evidence
Cargo.toml             Rust package metadata and desktop entry point
Cargo.lock             Pinned Rust dependency versions
```

Developer launch: `cargo build --workspace`, then `cargo run --bin hype-flow`. The installed command remains `hype-flow` for desktop and niri integration; its private flags are not a user-facing CLI workflow. If GTK4 layer-shell is installed under `~/.local`, include `~/.local/lib/pkgconfig` in `PKG_CONFIG_PATH` when invoking Cargo directly; Make sets this automatically.

The migration has two required stages: migrate and verify the application while retaining the C worker, then migrate the application-owned worker to Rust. The worker retains the pinned native inference libraries and process/network isolation. `make release` builds the complete Cargo workspace, including the worker used by first-use model setup. See the [migration design](docs/superpowers/specs/2026-11-21-rust-gtk4-design.md) for acceptance gates.

## Accuracy and limits

Parakeet Unified is the English model choice based on the upstream evaluation and the reference projects. See [provenance and evidence](docs/third_party.md). No model is universally best across voices, accents, noise, and vocabulary; evaluate representative speech before claiming parity with another product.

Long recordings use OpenWhispr's pause-aware 15-second chunk plan. **Cut silence before recognition** (Speech model → Audio processing, on by default) runs Silero voice detection inside the sandboxed worker with Handy's timing: threshold 0.3, 60 ms onset, 450 ms hangover, 450 ms kept before speech and 200 ms after. If it hears no speech, the whole recording is decoded rather than dropped. On the bundled sample with 10 seconds of added pauses, it kept 8.2 of 17.4 seconds, halved decode time, and produced no word errors. `backend.log` records how much audio each request kept. There is no LLM rewriting. Exact-zero digital silence is discarded. Transcript whitespace is normalized before optional text cleanup.

**Suppress background noise** (off by default) runs sherpa-onnx's GTCRN speech-enhancement model before voice detection. It adds about 0.5 seconds per dictation. In a test with white noise down to 3 dB SNR, Parakeet already transcribed without errors with and without it, so enable it for loud rooms and compare with `hype-flow-evaluate` on your own recordings. Both small models (Silero VAD 644 KB, GTCRN 536 KB) install with the speech runtime and are checksum-verified.

This runtime uses INT8 CPU inference. GPU acceleration is unavailable. **Vocabulary** accepts names and phrases. Their preferred capitalization is applied by text cleanup. Native decoder boosting is optional and off by default: OpenWhispr measured Parakeet Unified ignoring a boost of 3 and collapsing at 8, and boosting switches to slower modified beam search. When enabled, strength is adjustable from 0.5 to 3.0, and stronger bias can insert unwanted words. Compare it on your own recordings with `hype-flow-evaluate --boosting on` and `--boosting off`.

**Near-miss correction** snaps misheard words to your vocabulary when spelling distance, boosted for words that sound alike (Soundex), falls under the tolerance (default 0.18), and joins split words such as “Charge B” into “ChargeBee”. This is Handy's custom-word matcher; explicit correction rules always win over it.

**Text cleanup** applies your configured literal corrections and preferred vocabulary capitalization. Add one rule per line, for example `open ai => OpenAI`. Matching ignores case and uses whole words or phrases; it does not guess spelling or rewrite grammar. By default, filtering removes `um, uh, erm, umm, uhh`. Edit that list to remove other unwanted words or phrases. URLs, email addresses, quoted text, inline code, and unlisted uppercase acronyms are preserved. Adjacent repeated-word removal is optional and off by default; meaningful words such as “like” and “you know” remain unless you list them.

**Copy original** under **General** recovers the latest recognition result before text cleanup. If filtering removes everything, no paste occurs and the original remains recoverable. Disabling corrections or filtering takes effect without reloading the model.

Common preferences are available in the desktop window. Advanced paths, CPU threads, and microphone target are documented in [docs/config.toml](docs/config.toml). Model files live under `$XDG_DATA_HOME/hype-flow`, defaulting to `~/.local/share/hype-flow`. Previous default Whisper paths migrate to Parakeet; custom paths stay intact until a new model is explicitly installed. Applying a new model saves previous settings to `config.toml.previous` and retains other preferences. Old downloaded models are not deleted.

For custom post-processing, an executable you own at `~/.config/hype-flow/transcript-hook`, not writable by others, receives each cleaned transcript on standard input; its standard output is pasted instead. It has 30 seconds. If it fails, times out or is untrusted, the cleaned transcript is pasted and the status line says why.

## Paste and privacy

Text is copied with `wl-copy` and pasted with `wtype`. VS Code, Cursor and VSCodium use `Shift+Insert`, following OpenWhispr's editor-specific Linux route; both the clipboard and primary selection receive the transcript for these apps. About 30 known terminal applications use `Ctrl+Shift+V`; other applications use `Ctrl+V`. Embedded terminals and custom shortcuts may require automatic paste disabled. If focus changes during inference, Hype Flow returns focus to the window where dictation started, confirms it, and pastes there. If that window has closed, the text is copied without typing. Focus can still change between the final check and keystroke delivery.

After an automatic paste, Hype Flow restores the previous clipboard 0.8 seconds later, unless something new was copied in the meantime; an empty clipboard is cleared again. For the editor-specific `Shift+Insert` route, the previous primary selection is restored independently unless it changed. When text is only copied, the clipboard and the primary selection (`Shift+Insert`) keep the transcript. Hype Flow marks its clipboard writes sensitive, but clipboard managers may still save them. The latest original and cleaned transcripts stay in mode-0600 runtime files until application exit or replacement. A nonempty vocabulary list also occupies a private runtime file while its speech worker is loaded. Audio stays in memory and travels to the native worker through pipes. The worker runs inside a bubblewrap network namespace with only loopback, so it cannot reach any network; `backend.log` records whether the sandbox was active. **Copy transcript** recovers text after a paste failure. There is no transcript database.

The notch receives state and microphone levels over session D-Bus, never transcript text. Its existing weather and web-search features remain independent of offline dictation. Runtime diagnostics are stored at `$XDG_RUNTIME_DIR/hype-flow/backend.log`.

## Developer validation

```sh
make check
make check-python  # Previous implementation remains the migration oracle.
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
