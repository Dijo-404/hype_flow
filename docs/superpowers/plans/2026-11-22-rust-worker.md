# Rust Speech Worker Implementation Plan

**Goal:** Replace the application-owned C wrapper after the main Rust/GTK4
migration is complete and tested, keeping the pinned native inference API.

**Spec:** `../specs/2026-11-21-rust-gtk4-design.md`, phase 2.

## Gate and scope

- Do not implement this plan until phase-1 checks and their measured results
  are recorded in `docs/migration-results.md`.
- Keep separate-process and bubblewrap isolation, the `recognizer MODEL THREADS`
  interface, optional `--hotwords FILE SCORE`, `--vad FILE`, `--denoise FILE`,
  PCM16 framing, readiness/result JSON, model files, and runtime version.
- Keep the tested C worker available for rollback. Do not rewrite sherpa-onnx
  or ONNX Runtime. The Rust worker still loads their native libraries.

## Files and interfaces

- `worker/Cargo.toml`, `worker/src/main.rs`: package `hype-flow-worker`, binary
  `hype-flow-recognizer`, Rust 2024, using `libloading` for runtime-local API
  loading. Keep application builds independent of locally installed model files.
- `worker/src/bindings.rs`: generated, checked-in dynamic bindings from the
  exact installed sherpa-onnx 1.13.8 header. Record the header SHA-256 and command.
  Normal builds must not need a header download or a binding generator.
- `worker/tests/protocol.rs`: malformed frames, clean EOF, invalid options,
  native resource cleanup and speech-processing arguments against a fake API.
- `Cargo.toml`, `Makefile`, `tools/install.sh`: workspace builds/tests and
  installation of the worker beside the application executable.
- `src/setup.rs`: replace C compilation with staging the installed sibling
  Rust worker. Advance the worker version marker; preserve pinned archive,
  model, checksum, tokenizer and rollback behavior. A missing worker must fail
  clearly without replacing active assets.
- `tests/compare_workers.py`: real C/Rust protocol, transcript and repeated
  timing/memory comparison on the same local speech corpus and options.
- Existing setup/desktop fixtures and documentation: update the worker marker
  and compiler requirements while preserving the Python/C oracle.

## Steps

- [x] Record phase-1 passing checks, source review, measured benefits and limits.
- [x] Generate only the C API types/functions used by `recognizer.c`. Use
  `bindgen --dynamic-loading SherpaApi --dynamic-link-require-all` with a
  bounded function allowlist. Inspect generated layouts and ABI provenance.
- [x] Write protocol assertions before implementing the main loop. Requests
  contain a four-byte little-endian sample count followed by `count * 2`
  PCM bytes. Reject zero or more than 4800000 samples before allocating.
- [x] Implement option validation, retained CString lifetimes, and a small
  owned native-handle guard whose Drop invokes the matching API destroyer.
- [x] Load `lib/libsherpa-onnx-c-api.so` relative to the worker executable,
  initialize exactly the existing recognizer/VAD/denoise configs, and send
  the ready frame only after successful initialization.
- [x] Port VAD in 512-sample windows with 7200 samples of leading padding,
  3200 samples of trailing padding, no overlap duplication, and full-audio
  fallback when no speech is detected. Preserve denoiser fallback semantics.
- [x] Accept/copy audio into the native stream, release temporary sample
  buffers before decoding, and write/flush native result JSON with the
  existing 1048576-byte response cap. Keep diagnostics on stderr.
- [x] Run worker protocol tests, malformed-input cleanup tests, formatting,
  warning-clean linting, workspace tests and release builds.
- [x] Compare all real corpus outputs in greedy, boosting, VAD, denoising,
  and combined modes. Run at least five timing/memory repetitions, report
  medians and ranges, and verify cancellation/unload through the application.
- [x] Test setup and installed private-library loading in isolated roots.
  Verify application integration against a staged Rust worker and rollback
  to the known-good C worker before changing the live runtime.
- [x] Atomically install the Rust worker and version marker after verification,
  restart the idle application, and preserve settings/model/transcript recovery.
- [x] Record final results and remaining measurement limits. Keep legacy
  Python/C sources as explicit rollback references rather than deleting them.
