#!/usr/bin/env bash
# Assembles the macOS release package from a built KittyConnect binary (which
# already embeds both TailCat programs; see tools/build-tailcat-macos.sh).
# Run by build-macos.sh and the release workflow. Adapted from the Javacrafter
# Launcher's own, so the Launcher installs and updates it the same way.
#
#   tools/package-macos.sh <path/to/KittyConnect> <OutDir>
#
# Writes:
#   <Out>/KittyConnect/KittyConnect.app           the application bundle
#   <Out>/KittyConnect/Start KittyConnect.command  starts it, like the .bat on Windows
#   <Out>/KittyConnect/licenses/                    KittyConnect's, and everything in both TailCat programs
#   <Out>/KittyConnect/launcher-manifest.json      every file with its size and SHA-256
#   <Out>/KittyConnect-macos.zip                   what the Launcher downloads
#   <Out>/KittyConnect-macos.dmg                   for installing by hand (when hdiutil is there)
#   <Out>/release-entry-macos.json                 this release's entry for update-macos.json

set -euo pipefail

if [ "$#" -lt 2 ]; then
  echo "Usage: $0 <path/to/KittyConnect> <OutDir>" >&2
  exit 1
fi

EXE="$1"
OUT="$2"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
FOLDER="$OUT/KittyConnect"
APP="$FOLDER/KittyConnect.app"
ZIP_PATH="$OUT/KittyConnect-macos.zip"
DMG_PATH="$OUT/KittyConnect-macos.dmg"
MANIFEST="launcher-manifest.json"
EXE_REL="KittyConnect.app/Contents/MacOS/KittyConnect"

rm -rf "$FOLDER" "$ZIP_PATH" "$DMG_PATH"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$FOLDER/licenses"

cp "$EXE" "$APP/Contents/MacOS/KittyConnect"
chmod 755 "$APP/Contents/MacOS/KittyConnect"
cp "$ROOT/assets/KittyConnect.icns" "$APP/Contents/Resources/KittyConnect.icns"

# The microphone and local network texts are what macOS shows when it asks;
# without the microphone one, macOS silently gives the app no microphone.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>KittyConnect</string>
  <key>CFBundleExecutable</key>
  <string>KittyConnect</string>
  <key>CFBundleIconFile</key>
  <string>KittyConnect.icns</string>
  <key>CFBundleIdentifier</key>
  <string>com.javacrafter.kittyconnect</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>KittyConnect</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>
  <key>CFBundleVersion</key>
  <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>KittyConnect uses your microphone only while you're in voice in a VOIP room.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>KittyConnect connects directly to the people you share with when they're on your network, and finds Minecraft LAN worlds on this Mac.</string>
</dict>
</plist>
PLIST

cp "$ROOT/Start KittyConnect.command" "$FOLDER/Start KittyConnect.command"
chmod 755 "$FOLDER/Start KittyConnect.command"
cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$FOLDER/licenses/"
cp -R "$ROOT/vendor/macos/licenses/tailcat" "$FOLDER/licenses/tailcat"
cp -R "$ROOT/vendor/macos/licenses/kc-voip" "$FOLDER/licenses/kc-voip"

# Ad-hoc sign the bundle BEFORE hashing, so _CodeSignature and the signed
# binary match the hashes in launcher-manifest.json.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$APP"
fi

python3 - "$FOLDER" "$VERSION" "$EXE_REL" "$MANIFEST" "$ZIP_PATH" "$OUT/release-entry-macos.json" <<'PY'
import hashlib
import json
import os
import stat
import sys
import zipfile

folder, version, exe_rel, manifest_name, zip_path, entry_path = sys.argv[1:7]

files = []
for root, dirs, filenames in os.walk(folder):
    dirs[:] = [d for d in dirs if d.lower() not in ("data", "kittyconnectdata")]
    for name in filenames:
        if name == manifest_name or name == ".DS_Store":
            continue
        full = os.path.join(root, name)
        rel = os.path.relpath(full, folder).replace("\\", "/")
        h = hashlib.sha256()
        with open(full, "rb") as f:
            while chunk := f.read(65536):
                h.update(chunk)
        files.append({"path": rel, "size": os.path.getsize(full), "sha256": h.hexdigest().lower()})
files.sort(key=lambda item: item["path"])

with open(os.path.join(folder, manifest_name), "w", encoding="utf-8") as f:
    json.dump({"version": version, "exe": exe_rel, "files": files}, f, indent=2)

with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for rel in [item["path"] for item in files] + [manifest_name]:
        src = os.path.join(folder, rel)
        st = os.stat(src)
        zi = zipfile.ZipInfo.from_file(src, arcname=rel)
        zi.compress_type = zipfile.ZIP_DEFLATED
        zi.create_system = 3  # Unix, so the modes below count
        mode = stat.S_IMODE(st.st_mode)
        if rel == exe_rel or ".app/Contents/MacOS/" in rel or rel.endswith(".command"):
            mode |= 0o755
        zi.external_attr = (stat.S_IFREG | mode) << 16
        with open(src, "rb") as f:
            zf.writestr(zi, f.read())

zip_hash = hashlib.sha256()
with open(zip_path, "rb") as f:
    while chunk := f.read(65536):
        zip_hash.update(chunk)

with open(entry_path, "w", encoding="utf-8") as f:
    json.dump({
        "version": version,
        "exe": exe_rel,
        "package": {"name": os.path.basename(zip_path), "size": os.path.getsize(zip_path), "sha256": zip_hash.hexdigest().lower()},
        "files": files,
    }, f, indent=2)
PY

# A disk image with an Applications shortcut, for installing without the Launcher.
if command -v hdiutil >/dev/null 2>&1; then
  STAGE="$OUT/dmg-stage"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  cp -R "$APP" "$STAGE/KittyConnect.app"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname "KittyConnect" -fs HFS+ -srcfolder "$STAGE" -ov -format UDZO "$DMG_PATH" >/dev/null || true
  rm -rf "$STAGE"
fi

echo "Packaged KittyConnect $VERSION for macOS -> $ZIP_PATH"
