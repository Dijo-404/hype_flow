# Reuse and provenance

Hype Flow uses sherpa-onnx with NVIDIA Parakeet Unified English INT8. The desktop is a native GTK application. It does not bundle OpenWhispr's Electron application or Handy's Tauri application.

## OpenWhispr reference

Reference checkout: `007826d936af15e89291c1106042f1b280a75933`.

The selected model, pinned sherpa-onnx version, and four required model files follow `src/models/modelRegistryData.json`, `src/helpers/parakeetModelInfo.js`, and `scripts/download-sherpa-onnx.js`. The CPU runtime is version **1.13.8**, using the upstream `shared-no-tts` archive, as in the reference.

`src/dictation/segments.py` ports `src/helpers/parakeetSegmentation.js` to PCM16. It moves 15-second chunk boundaries toward pauses without dropping, duplicating, or reordering samples, and absorbs tails up to 0.5 seconds. The native SDK handles inference; Hype Flow's small C worker keeps the model loaded and exchanges PCM and results over anonymous pipes. It opens no HTTP or WebSocket server.

The reference disables hotword boosting for Parakeet Unified because its benefit has not been validated and high scores damaged recognition (`src/utils/sherpaHotwords.js`). Hype Flow exposes it at the user's request, using the [documented transducer hotword path](https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html): `modified_beam_search`, four active paths, BPE encoding, and a bounded score of 0.5–3.0 (default 1.5). An empty list or disabled boost keeps greedy decoding. This is native decoder biasing, not a guarantee that Unified recognizes a listed term more accurately.

Text cleanup is local, literal Python processing: configured corrections, vocabulary capitalization, explicit word filters, and optional adjacent repeated-word removal. It adds no model or spelling dependency. Corrections and default filler filtering are enabled; repeated-word removal is disabled. The original pre-cleanup result remains separately recoverable.

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
- The helper is compiled locally against the pinned C header. No Python model framework, Torch, CUDA toolkit, or WebSocket dependency is needed.

Sherpa-onnx's Apache 2.0 license: [licenses/sherpa_onnx.txt](licenses/sherpa_onnx.txt). ONNX Runtime's MIT notice is retained at [licenses/onnxruntime.txt](licenses/onnxruntime.txt), from its [upstream license](https://github.com/microsoft/onnxruntime/blob/main/LICENSE).

## Handy and earlier Whisper backend

Handy reference checkout: `4807c21a7bb9a3bf2d52578a9b541e165b6bc8b9`. The overlay follows `src-tauri/src/overlay.rs`: a layer-shell surface without keyboard focus or an exclusive zone. Handy's MIT license: [licenses/handy.txt](licenses/handy.txt).

The initial prototype used OpenWhispr's whisper.cpp release `0.0.11` with full large-v3 weights. Parakeet replaces that runtime as the default. Existing model downloads are left intact, and previous default config paths migrate in memory while custom paths and preferences are preserved. The earlier whisper.cpp MIT notice is retained at [licenses/whisper_cpp.txt](licenses/whisper_cpp.txt).
