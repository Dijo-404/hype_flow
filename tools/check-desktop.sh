#!/bin/sh
# Run only in an isolated D-Bus session and a controlled Wayland compositor.
set -eu
project=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project"
cargo_bin=${CARGO:-cargo}
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
"$cargo_bin" test --test rust_desktop -- --ignored --test-threads=1
"$cargo_bin" build --bin hype-flow
python3 tests/ui_driver.py "$project/target/debug/hype-flow"
