# Reuse and provenance

Hype Flow uses sherpa-onnx with NVIDIA Parakeet 110M CTC English INT8. The native
GTK desktop also supports the 110M transducer export and Parakeet Unified.

## OpenWhispr reference

Reference checkout: `007826d936af15e89291c1106042f1b280a75933`.

The Unified profile and its four required model files follow `src/models/modelRegistryData.json`, `src/helpers/parakeetModelInfo.js`, and `scripts/download-sherpa-onnx.js`. The CPU runtime remains version **1.13.8**, using the upstream `shared-no-tts` archive.

`src/segments.rs` adapts `src/helpers/parakeetSegmentation.js` to PCM16. It prefers pauses when choosing chunk boundaries and caps each piece at the configured duration. The native SDK handles inference, and Hype Flow's Rust worker exchanges PCM and results over anonymous pipes.

The reference disables hotword boosting for Parakeet Unified because its benefit has not been validated and high scores damaged recognition (`src/utils/sherpaHotwords.js`). Hype Flow exposes it at the user's request, using the [documented transducer hotword path](https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html): `modified_beam_search`, four active paths, BPE encoding, and a bounded score of 0.5–3.0 (default 1.5). An empty list or disabled boost keeps greedy decoding. This is native decoder biasing, not a guarantee that Unified recognizes a listed term more accurately.

The Rust cleanup code ports the previous Python implementation's configured corrections, vocabulary capitalization, explicit word filters, and optional adjacent repeated-word removal. Corrections and default filler filtering are enabled by default. Repeated-word removal is opt-in. The original recognition remains recoverable.

Linux paste routing also follows `src/helpers/clipboard.js` at the reference commit above: VS Code and Cursor use Shift+Insert with the transcript available in both clipboard selections. Hype Flow restores each selection independently and retains its cancellation and focus checks. This route also covers VSCodium.

The settings window adapts `src/index.css`, `ControlPanelSidebar.tsx`, and `ui/SettingsSection.tsx`: sidebar navigation, charcoal surfaces, blue accents, and compact panels. It uses installed Noto Sans rather than the proprietary brand fonts.

OpenWhispr's MIT license: [licenses/openwhispr.txt](licenses/openwhispr.txt).

## Parakeet model

Base model: [NVIDIA Parakeet Unified EN 0.6B](https://huggingface.co/nvidia/parakeet-unified-en-0.6b). The upstream model card links its governing NVIDIA Open Model License terms. Recognition is English only, with punctuation and capitalization.

The quantized export comes from the official [sherpa-onnx ASR model release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models):

- Archive: `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2`.
- Archive size: **501,350,460 bytes**.
- Archive SHA-256: `99f63605b3a85a54c250c0869670a687b7d6598a47bf2421515e1f839a76e150`.
- Installed model files: **663,043,117 bytes** total (663 MB decimal, approximately 632 MiB).
- Encoder: 654,040,552 bytes; decoder: 7,257,753 bytes; joiner: 1,735,860 bytes; tokens: 8,952 bytes.

Vocabulary support additionally downloads a 251,056-byte [SentencePiece tokenizer](https://huggingface.co/eschmidbauer/parakeet-unified-en-0.6b-onnx/blob/7a16ff98b72e5b0beba6f182a5357c704d977b15/onnx_int8/tokenizer.model) from the community ONNX export at revision `7a16ff98b72e5b0beba6f182a5357c704d977b15`. Its SHA-256 is `07d4e5a63840a53ab2d4d106d2874768143fb3fbdd47938b3910d2da05bfb0a9`. Setup verifies the checksum and checks its complete piece inventory against the installed `tokens.txt`, excluding the transducer blank. A bounded protobuf reader extracts the original SentencePiece scores into a 9,961-byte `bpe.vocab`. Total installed model files remain below 1 GB at **663,053,078 bytes**; the tokenizer download stays in the setup cache. The resource comes from the same Parakeet model family; governing model terms remain linked above.

NVIDIA's reported 5.91% offline WER describes its upstream model evaluation, not a separately established accuracy guarantee for this INT8 export or the user's speech. [Handy's English recommendations](https://models.handy.computer/recommendations/) also select Unified for their English accuracy/speed targets. These support the model choice, not a universal claim that it beats every model on every workload.

## Runtime

Upstream: [sherpa-onnx v1.13.8](https://github.com/k2-fsa/sherpa-onnx/releases/tag/v1.13.8).

- Archive: `sherpa-onnx-v1.13.8-linux-x64-shared-no-tts.tar.bz2`.
- SHA-256: `d0f96c8b65c6cd0974fada22737e337de81bc8cd2abbec2e39caf358b1eec5fc`.
- Only native libraries and the C header are installed; the bundled command-line programs are not copied.
- The Rust worker uses checked-in dynamic API bindings generated from the pinned C header. First-use setup stages the shipped worker; it does not compile C. No Python model framework, Torch, CUDA toolkit, or WebSocket dependency is needed.

Sherpa-onnx's Apache 2.0 license: [licenses/sherpa_onnx.txt](licenses/sherpa_onnx.txt). ONNX Runtime's MIT notice is retained at [licenses/onnxruntime.txt](licenses/onnxruntime.txt), from its [upstream license](https://github.com/microsoft/onnxruntime/blob/main/LICENSE).

## Handy and earlier Whisper backend

Handy reference checkout: `4807c21a7bb9a3bf2d52578a9b541e165b6bc8b9`. The overlay follows `src-tauri/src/overlay.rs`: a layer-shell surface without keyboard focus or an exclusive zone. Handy's MIT license: [licenses/handy.txt](licenses/handy.txt).

The initial prototype used OpenWhispr's whisper.cpp release `0.0.11` with full large-v3 weights. Parakeet replaces that runtime as the default. Existing model downloads are left intact, and previous default config paths migrate in memory while custom paths and preferences are preserved. The earlier whisper.cpp MIT notice is retained at [licenses/whisper_cpp.txt](licenses/whisper_cpp.txt).

## Rust application migration

The application and its speech worker have been ported to Rust, with GTK4 for
the desktop interface. `Cargo.lock` records the
exact Rust dependency versions. GTK4 and GTK4 layer-shell replace the prior
GTK3 bindings; model and inference-runtime provenance above remains unchanged.
The Rust worker uses the same pinned sherpa-onnx API and ONNX Runtime native
libraries as the previous C worker. [Migration results](migration-results.md)
record verification and the revision containing the original source.

Clipboard sensitivity uses `wl-copy --sensitive` from wl-clipboard 2.3 or
later. Existing model notices and reference-project credits remain applicable.

### Reproducing the worker bindings

The C API header SHA-256 is
`2a1b95084be8fd1deb3228fcad2fd3f7f0258b64582f7402281ec174c7b7f4ce`.
All 33 concrete generated struct sizes/alignments and 155 field offsets were
compared independently to a C compiler using this header. Normal builds use
the checked-in bindings and require neither a generator nor installed model files.

```sh
cargo install bindgen-cli --version 0.72.1 --locked
sh tools/generate-worker-bindings.sh /path/to/sherpa-onnx/c-api/c-api.h
```

The script verifies the header digest, derives the 21-function allowlist from
the Rust worker's API calls, and runs bindgen.
Regeneration was verified byte-for-byte against the checked-in output.

## Evaluation-only Whisper English normalizer

`tools/normalizers/english.py`, `basic.py` and `english.json` derive from
`whisper.cpp/tests/librispeech/normalizers` at commit
`5eaee0cc9024eb9e9a99469199920c8b7f5cb9ec`. The code originates in OpenAI
Whisper, copyright 2022 OpenAI, under the MIT license retained in
[`tools/normalizers/LICENSE`](../tools/normalizers/LICENSE).
The English number and spelling logic is retained; the three-element sliding
window uses standard-library `zip`, and unused grapheme-splitting helpers are
omitted, avoiding Python package dependencies. Rust evaluation embeds these
sources and runs them through Python only for benchmark scoring.

## Common-word vocabulary guard

`assets/common_words.txt` contains the first 5,000 entries of
[high-frequency-vocabulary](https://github.com/arstgit/high-frequency-vocabulary)'s
`10k.txt`, derived from Peter Norvig's word frequency compilation. Rust embeds
the list for vocabulary cleanup. See the [MIT license](../assets/common_words.LICENSE)
and [source details](../assets/common_words.SOURCE).

The conservative fuzzy gate and legitimate repeated-word exceptions follow
Voxtype's design. Exact matches, unique-target matching, bounded spelling
edits, Soundex, and an exact consonant-skeleton check preserve known split-name
and vowel-heavy-name cases. Soundex no longer reduces the edit-distance score.
Fuzzy matching remains opt-in; no measured corpus-wide accuracy gain is claimed.

## Conservative English number and restart processing

`src/itn.rs`, `src/restarts.rs` and the word-diff helper in `src/text_diff.rs`
adapt Voxtype's guarded processing at commit
`4d26c1f40a71f4e9dbcedf13f1f2a887feefd96c`. Copyright 2025 Peter Jackson; the MIT license is retained in
[licenses/voxtype.txt](licenses/voxtype.txt). They retain bounded restart anchors,
function-word exclusions, whole-rewrite reconciliation and ordinary-word guards.
Number conversion uses Apache-2.0 `text-processing-rs` 0.2.2, locked in
`Cargo.lock` (crate source revision `34a9b62d64d95844d1916774054909fccee88f9a`); its license is retained in
[licenses/text_processing_rs.txt](licenses/text_processing_rs.txt). This is a
pure-Rust port of NeMo's English number grammars; it requires no additional model
process. Formatting and restart editing are opt-in, and protected URLs, addresses,
quotes and inline code bypass them.

## Parakeet 110M profiles

The 110M profiles use [NVIDIA and Suno's Parakeet TDT-CTC 110M English
model](https://huggingface.co/nvidia/parakeet-tdt_ctc-110m), licensed under
[Creative Commons Attribution 4.0](https://creativecommons.org/licenses/by/4.0/).
Sherpa-onnx exports the CTC and transducer branches to ONNX and quantizes
them to INT8. The same attribution applies to both exports.

The default CTC profile uses the [official sherpa-onnx CTC archive](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_ctc_110m-en-36000-int8.tar.bz2).
The 104,337,827-byte download has SHA-256
`17f945007b52ccd8b7200ffc7c5652e9e8e961dfdf479cefcabd06cf5703630b`,
verified against the upstream release asset digest. The installed model and
token table total 131,662,124 bytes.

The TDT profile uses the transducer export.

- Official asset: [sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8.tar.bz2](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8.tar.bz2).
- Download size: **108,035,095 bytes**.
- Archive SHA-256: `f628312e9fdf8686374cb01a69425c41732529d540860311f16f37cbc32cfe9b`, checked against the upstream GitHub release asset digest.
- Installed encoder, decoder, joiner and tokens: **136,490,421 bytes**.
- Both 110M profiles use 80 features. Unified uses 128.

Both 110M profiles use greedy decoding. Configured literal corrections and
spelling and capitalization preferences apply after recognition.
