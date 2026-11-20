#!/usr/bin/env bash
# Build a universal Cleanup Mac.app, sign it, notarize and staple it, zip it, and publish a GitHub release.
#
#   SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" NOTARY_PROFILE=<profile> scripts/release.sh
#
# NOTARY_PROFILE names credentials saved once with `xcrun notarytool store-credentials <profile> --team-id TEAMID`.
# The release is tagged v<version from Cargo.toml>; a tag that already exists stops the script.
set -euo pipefail

cd "$(dirname "$0")/.."
: "${SIGN_IDENTITY:?set SIGN_IDENTITY to a Developer ID Application identity}"
: "${NOTARY_PROFILE:?set NOTARY_PROFILE to a notarytool keychain profile}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"

if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  echo "release $TAG already exists; bump the version in Cargo.toml first" >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree has uncommitted changes; commit them so the release matches a commit" >&2
  exit 1
fi

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --target "$target"
done

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
BUNDLE="$OUT/Cleanup Mac.app"
mkdir -p "$BUNDLE/Contents/MacOS"
lipo -create -output "$BUNDLE/Contents/MacOS/cleanup-mac" \
  target/aarch64-apple-darwin/release/cleanup-mac target/x86_64-apple-darwin/release/cleanup-mac
scripts/info-plist.sh "$VERSION" > "$BUNDLE/Contents/Info.plist"

codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$BUNDLE"
codesign --verify --strict "$BUNDLE"

ZIP="$OUT/Cleanup-Mac-$VERSION.zip"
ditto -c -k --keepParent "$BUNDLE" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$BUNDLE"
spctl --assess --type execute "$BUNDLE"
# Zip again so the download carries the stapled ticket and opens offline.
rm "$ZIP"
ditto -c -k --keepParent "$BUNDLE" "$ZIP"
(cd "$OUT" && shasum -a 256 "$(basename "$ZIP")" > "$(basename "$ZIP").sha256")

git push origin HEAD
gh release create "$TAG" "$ZIP" "$ZIP.sha256" --repo "$REPO" --target "$(git rev-parse HEAD)" \
  --title "Cleanup Mac $VERSION" --notes-file - <<NOTES
Universal (Apple silicon and Intel), signed with a Developer ID and notarized by Apple. macOS 13 or later.

1. Download \`Cleanup-Mac-$VERSION.zip\`, unzip it, and move **Cleanup Mac** to Applications.
2. Open it and grant Full Disk Access when it asks; without it macOS hides parts of ~/Library from the scan.

The command line is the same binary:
\`ln -s "/Applications/Cleanup Mac.app/Contents/MacOS/cleanup-mac" /usr/local/bin/cleanup-mac\`, then \`cleanup-mac --help\`.
NOTES
echo "Released $TAG: $(gh release view "$TAG" --repo "$REPO" --json url --jq .url)"
