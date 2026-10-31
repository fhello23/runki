#!/bin/sh
# Wraps a built runki binary in Runki.app, the double-clickable macOS app.
#
#   scripts/bundle-macos.sh <runki binary> <output dir>
#
# The version comes from Cargo.toml. The bundle is ad-hoc signed: enough for
# macOS to run it, not notarized (see "Install" in the README).
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <runki binary> <output dir>" >&2
    exit 2
fi
binary=$1
out=$2
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)

app="$out/Runki.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/runki"

# Icon: every size macOS asks for, from the 1024px PNG.
iconset=$(mktemp -d)/Runki.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$root/assets/icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$root/assets/icon.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Runki.icns"
rm -rf "$(dirname "$iconset")"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Runki</string>
    <key>CFBundleDisplayName</key>
    <string>Runki</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.fhello23.runki</string>
    <key>CFBundleExecutable</key>
    <string>runki</string>
    <key>CFBundleIconFile</key>
    <string>Runki</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.education</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

codesign --force --sign - "$app"
echo "$app"
