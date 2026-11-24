#!/bin/sh
# Install the staged Rust application without changing compositor settings.
set -eu

project=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
prefix=${HYPE_FLOW_PREFIX:-"$HOME/.local"}
data=${XDG_DATA_HOME:-"$HOME/.local/share"}/hype-flow
applications=${XDG_DATA_HOME:-"$HOME/.local/share"}/applications
icons=${XDG_DATA_HOME:-"$HOME/.local/share"}/icons/hicolor/scalable/apps
private_lib="$prefix/share/hype-flow/lib"
mkdir -p "$prefix/bin" "$private_lib" "$data/rollback" "$applications" "$icons"
test -x "$project/target/release/hype-flow"
test -x "$project/target/release/hype-flow-evaluate"
test -x "$project/target/release/hype-flow-recognizer"

if [ -f "$prefix/bin/hype-flow" ] && [ ! -f "$data/rollback/hype-flow" ]; then
    cp -p "$prefix/bin/hype-flow" "$data/rollback/hype-flow"
fi
if [ -f "$applications/dev.hype.Flow.desktop" ] && [ ! -f "$data/rollback/dev.hype.Flow.desktop" ]; then
    cp -p "$applications/dev.hype.Flow.desktop" "$data/rollback/dev.hype.Flow.desktop"
fi
if pkg-config --exists gtk4-layer-shell-0; then
    library_dir=$(pkg-config --variable=libdir gtk4-layer-shell-0)
    library_stage=$(mktemp "$private_lib/.layer-shell.XXXXXX")
    trap 'rm -f "$library_stage"' EXIT HUP INT TERM
    cp -L "$library_dir/libgtk4-layer-shell.so.0" "$library_stage"
    chmod 644 "$library_stage"
    mv -f "$library_stage" "$private_lib/libgtk4-layer-shell.so.0"
    trap - EXIT HUP INT TERM
fi
for name in hype-flow-recognizer hype-flow hype-flow-evaluate; do
    staged=$(mktemp "$prefix/bin/.$name.XXXXXX")
    trap 'rm -f "$staged"' EXIT HUP INT TERM
    install -m755 "$project/target/release/$name" "$staged"
    mv -f "$staged" "$prefix/bin/$name"
    trap - EXIT HUP INT TERM
done
install -m644 "$project/assets/logo.svg" "$icons/dev.hype.Flow.svg"
escaped=$(printf '%s' "$prefix/bin/hype-flow" | sed 's/[\\"`$]/\\&/g; s/\\/\\\\/g; s/%/%%/g')
desktop_stage=$(mktemp "$applications/.hype-flow.desktop.XXXXXX")
trap 'rm -f "$desktop_stage"' EXIT HUP INT TERM
cat >"$desktop_stage" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Hype Flow
Comment=Offline voice dictation for niri
Exec=/usr/bin/env "$escaped"
Icon=dev.hype.Flow
Terminal=false
Categories=AudioVideo;Audio;
StartupNotify=true
StartupWMClass=dev.hype.Flow
DESKTOP
chmod 644 "$desktop_stage"
mv -f "$desktop_stage" "$applications/dev.hype.Flow.desktop"
trap - EXIT HUP INT TERM
printf 'Installed %s\n' "$prefix/bin/hype-flow"
printf 'Previous launcher retained in %s\n' "$data/rollback"
