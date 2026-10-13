#!/bin/sh
# Makes M-Flasher.app (ad-hoc signed: no Apple certificate) and the disk image
# M-Flasher-VERSION-macos.dmg from an m-flasher binary. macOS only.
#   macos/package.sh target/release/m-flasher 0.3.0 [OUTDIR]
set -e
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
version=$2
here=$(cd "$(dirname "$0")" && pwd)
out=${3:-$here/../target/macos}
rm -rf "$out" && mkdir -p "$out" && out=$(cd "$out" && pwd)

app="$out/M-Flasher.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/M-Flasher"
chmod 755 "$app/Contents/MacOS/M-Flasher"
cp "$here/../LICENSE" "$app/Contents/Resources/"

# the icon, from the 1024-pixel PNG
iconset="$out/AppIcon.iconset"
mkdir "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s "$here/icon.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$here/icon.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -r "$iconset"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>io.github.jvitkauskas.m-flasher</string>
  <key>CFBundleName</key><string>M-Flasher</string>
  <key>CFBundleDisplayName</key><string>M-Flasher</string>
  <key>CFBundleExecutable</key><string>M-Flasher</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist"
codesign --force --sign - "$app"
codesign --verify --deep --strict --verbose=2 "$app"

# the disk image: the app, a shortcut to Applications, and the instructions
# in the background (dmgbuild, from a virtual environment with pinned hashes;
# Python 3.10 or newer, PYTHON=... to choose; DMGBUILD=dmgbuild==1.6.5 for an
# older Python, unpinned)
"${PYTHON:-python3}" -m venv "$out/venv"
if [ -n "$DMGBUILD" ]; then
  "$out/venv/bin/pip" install -q "$DMGBUILD"
else
  "$out/venv/bin/pip" install -q --require-hashes -r "$here/requirements-dmg.txt"
fi
dmg="$out/M-Flasher-$version-macos.dmg"
"$out/venv/bin/python" - "$app" "$dmg" "$here/dmg/background.png" <<'EOF'
import sys
import dmgbuild
app, dmg, background = sys.argv[1:]
dmgbuild.build_dmg(dmg, 'M-Flasher', settings={
    'format': 'UDZO',
    'filesystem': 'HFS+',
    'files': [app],
    'symlinks': {'Applications': '/Applications'},
    'icon': app + '/Contents/Resources/AppIcon.icns',
    # background@2x.png next to it makes it sharp on Retina screens
    'background': background,
    # the icons where background.svg leaves room for them
    'icon_locations': {'M-Flasher.app': (184, 210), 'Applications': (536, 210)},
    # 720 x 500 of content, plus Finder's title bar
    'window_rect': ((160, 160), (720, 528)),
    'default_view': 'icon-view',
    'icon_size': 96,
    'text_size': 14,
    'show_icon_preview': False,
    'show_status_bar': False,
    'show_toolbar': False,
    'show_pathbar': False,
    'show_sidebar': False,
})
EOF
rm -rf "$out/venv"
hdiutil verify "$dmg"
ls -l "$dmg"
