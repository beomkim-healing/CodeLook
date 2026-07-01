#!/bin/bash
# Build CodeLook.app — a double-clickable macOS application bundle.
set -euo pipefail
cd "$(dirname "$0")"

APP="CodeLook.app"
CONTENTS="$APP/Contents"

echo "▶ Building release binary…"
cargo build --release

echo "▶ Assembling $APP …"
rm -rf "$APP"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"

cp target/release/codelook "$CONTENTS/MacOS/codelook"
chmod +x "$CONTENTS/MacOS/codelook"
cp Info.plist "$CONTENTS/Info.plist"
cp assets/AppIcon.icns "$CONTENTS/Resources/AppIcon.icns"

# Ad-hoc code signature so macOS lets it run locally.
codesign --force --deep --sign - "$APP" 2>/dev/null || true

# Refresh Finder's icon cache for this bundle.
touch "$APP"

echo "✓ Done → $(pwd)/$APP"
