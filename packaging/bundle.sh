#!/usr/bin/env bash
# Build Subbier.app for local use. `cargo run -p subbier-macos` already runs as
# a menu bar item without a bundle; the bundle exists for /Applications and the
# Finder icon. cargo-bundle cannot set LSUIElement; use cargo-packager instead.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="${1:-release}"
app="$root/target/$profile/Subbier.app"

cargo build -p subbier-macos $([ "$profile" = release ] && echo --release)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/$profile/subbier-menubar" "$app/Contents/MacOS/Subbier"
cp "$root/assets/Subbier.icns" "$app/Contents/Resources/Subbier.icns"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Subbier</string>
	<key>CFBundleDisplayName</key><string>subbier</string>
	<key>CFBundleExecutable</key><string>Subbier</string>
	<key>CFBundleIdentifier</key><string>com.github.anowell.subbier</string>
	<key>CFBundleIconFile</key><string>Subbier.icns</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<!-- Menu bar only: no dock icon, no main window. The app also sets this at
	     runtime via setActivationPolicy(Accessory), so an unbundled binary
	     behaves the same. -->
	<key>LSUIElement</key><true/>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature. Enough for Gatekeeper to run it locally; not notarization.
codesign --force --deep -s - "$app"
echo "built $app"
