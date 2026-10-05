#!/usr/bin/env bash
# Renders packaging/macos/AppIcon.svg into packaging/macos/AppIcon.icns, and the tray icons in
# packaging/tray/ into assets/tray/*.png (embedded in the app).
#
# Needs an SVG renderer: `resvg` (cargo install resvg) or `rsvg-convert` (brew install librsvg).
# Set SVG_RENDERER to a binary taking `<in.svg> <out.png> <size>` to use something else.
set -euo pipefail

cd "$(dirname "$0")/.."
svg=packaging/macos/AppIcon.svg
iconset="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$iconset"

render_file() { # <in.svg> <size> <out.png>
    if [[ -n "${SVG_RENDERER:-}" ]]; then
        "$SVG_RENDERER" "$1" "$3" "$2"
    elif command -v resvg >/dev/null; then
        resvg --width "$2" --height "$2" "$1" "$3"
    elif command -v rsvg-convert >/dev/null; then
        rsvg-convert --width "$2" --height "$2" "$1" --output "$3"
    else
        echo "No SVG renderer found; install resvg or librsvg." >&2
        exit 1
    fi
}
render() { render_file "$svg" "$1" "$2"; }

for size in 16 32 128 256 512; do
    render "$size" "$iconset/icon_${size}x${size}.png"
    render "$((size * 2))" "$iconset/icon_${size}x${size}@2x.png"
done
iconutil --convert icns "$iconset" --output packaging/macos/AppIcon.icns
echo "Wrote packaging/macos/AppIcon.icns"

# Tray icons. macOS uses monochrome template glyphs, drawn at 22 pt and rendered at 2x.
# Windows (32 px) and Linux (64 px) use the colored app icon, plus a variant with a red
# "recording" badge.
mkdir -p assets/tray
render_file packaging/tray/template.svg 44 assets/tray/template.png
render_file packaging/tray/template-recording.svg 44 assets/tray/template-recording.png
recording_svg="$(dirname "$iconset")/AppIcon-recording.svg"
sed 's#</svg>#  <circle cx="812" cy="212" r="150" fill="\#FF3B30" stroke="\#FFFFFF" stroke-width="44"/>\n</svg>#' \
    "$svg" > "$recording_svg"
for size in 32 64; do
    render_file "$svg" "$size" "assets/tray/color-$size.png"
    render_file "$recording_svg" "$size" "assets/tray/color-$size-recording.png"
done
echo "Wrote assets/tray/*.png"
