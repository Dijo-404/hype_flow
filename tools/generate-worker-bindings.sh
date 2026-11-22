#!/bin/sh
# Optional maintenance step; normal builds use the checked-in pinned bindings.
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
header=${1:?'Pass the sherpa-onnx 1.13.8 c-api.h path'}
expected=2a1b95084be8fd1deb3228fcad2fd3f7f0258b64582f7402281ec174c7b7f4ce
actual=$(sha256sum "$header" | cut -d ' ' -f 1)
[ "$actual" = "$expected" ] || { printf '%s\n' 'Rejected: header checksum does not match sherpa-onnx 1.13.8' >&2; exit 1; }
[ "$(bindgen --version)" = 'bindgen 0.72.1' ] || { printf '%s\n' 'Install bindgen-cli 0.72.1 before regenerating' >&2; exit 1; }
symbols=$(rg -o 'SherpaOnnx[A-Za-z0-9_]+\(' "$project/src/dictation/native/recognizer.c" | sed 's/(//' | sort -u | paste -sd '|' -)
bindgen "$header" \
    --allowlist-function "^($symbols)$" --allowlist-var '^$' \
    --no-layout-tests --with-derive-default \
    --dynamic-loading SherpaApi --dynamic-link-require-all \
    --rust-target 1.85 --rust-edition 2024 \
    --raw-line "// Generated from sherpa-onnx 1.13.8 c-api.h; SHA-256 $expected." \
    --output "$project/worker/src/bindings.rs"
