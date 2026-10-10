#!/usr/bin/env bash
# Builds GetYourMusic.app. macOS only grants system-audio recording (and shows its permission
# prompt) to a bundled app with a usage description, so run the app through this bundle.
#
# Usage: scripts/bundle-macos.sh [debug|release] [--open]
set -euo pipefail

cd "$(dirname "$0")/.."
profile="${1:-debug}"
open_after="${2:-}"

if [[ "$profile" == "release" ]]; then
    cargo build --release
else
    cargo build
fi

version="$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="getyourmusic"))')"
target="target/$profile"
app="$target/GetYourMusic.app"
contents="$app/Contents"

# The newest adapter build (cargo's build-dir layout differs between toolchains).
framework="$(find "$target/build" -maxdepth 6 -type d -name MediaRemoteAdapter.framework -path '*gym-platform*' -prune \
    -exec stat -f '%m %N' {} + 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)"
if [[ -z "$framework" ]]; then
    echo "MediaRemoteAdapter.framework not found; was gym-platform built?" >&2
    exit 1
fi
test_client="$(dirname "$framework")/MediaRemoteAdapterTestClient"

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources" "$contents/Frameworks" "$contents/Helpers"
cp "$target/getyourmusic" "$contents/MacOS/GetYourMusic"
cp -R "$framework" "$contents/Frameworks/"
cp "$test_client" "$contents/Helpers/"
cp crates/gym-platform/vendor/mediaremote-adapter/bin/mediaremote-adapter.pl "$contents/Resources/"
cp crates/gym-platform/vendor/mediaremote-adapter/LICENSE "$contents/Resources/mediaremote-adapter-LICENSE"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$contents/Info.plist"
if [[ -f packaging/macos/AppIcon.icns ]]; then
    cp packaging/macos/AppIcon.icns "$contents/Resources/AppIcon.icns"
    /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$contents/Info.plist"
fi

# Ad-hoc signature for local use; distribution needs a Developer ID identity and notarization.
codesign --force --deep --sign "${CODESIGN_IDENTITY:--}" "$app"
echo "Built $app"

if [[ "$open_after" == "--open" ]]; then
    open "$app"
fi
