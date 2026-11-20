#!/usr/bin/env bash
# Build Cleanup Mac, bundle it as /Applications/Cleanup Mac.app, sign it, and link the CLI to the bundled binary.
#
#   SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" scripts/install-app.sh
#
# Sign with a stable identity: macOS ties the Full Disk Access grant to it, so the grant survives reinstalls.
# Without SIGN_IDENTITY the app is signed ad hoc and the grant has to be given again after every install.
set -euo pipefail

cd "$(dirname "$0")/.."
APP="/Applications/Cleanup Mac.app"
BIN_LINK="$HOME/.cargo/bin/cleanup-mac"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
IDENTITY="${SIGN_IDENTITY:--}"

cargo build --release

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
BUNDLE="$STAGE/Cleanup Mac.app"
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/release/cleanup-mac "$BUNDLE/Contents/MacOS/cleanup-mac"
cat > "$BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Cleanup Mac</string>
  <key>CFBundleDisplayName</key><string>Cleanup Mac</string>
  <key>CFBundleIdentifier</key><string>local.cleanup-mac</string>
  <key>CFBundleExecutable</key><string>cleanup-mac</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$BUNDLE"
codesign --verify --strict "$BUNDLE"

# Replace the installed app only after the new bundle is built and signed.
pkill -xf "$APP/Contents/MacOS/cleanup-mac" 2>/dev/null || true  # the window only, never a running CLI sweep
rm -rf "$APP"
mv "$BUNDLE" "$APP"

mkdir -p "$(dirname "$BIN_LINK")"
ln -sfn "$APP/Contents/MacOS/cleanup-mac" "$BIN_LINK"

echo "Installed $APP $VERSION (signed: $IDENTITY)"
echo "CLI: $BIN_LINK -> $("$BIN_LINK" --version)"
[ "$IDENTITY" = "-" ] && echo "Signed ad hoc: grant Full Disk Access again in System Settings if the app asks."
exit 0
