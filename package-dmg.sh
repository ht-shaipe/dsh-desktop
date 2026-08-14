#!/usr/bin/env bash
# Package dsh-desktop as a distributable .dmg (macOS).
# Requires Xcode Command Line Tools (hdiutil) and a GUI session for the
# cosmetic window layout (the layout step is non-fatal if it can't run).
set -euo pipefail

APP_NAME="dsh-desktop"
VOL_NAME="DeepSeek dsh Web"
DMG_NAME="dsh-desktop.dmg"
RW_DMG="dsh-desktop-rw.dmg"
STAGING="dmg-staging"
MNT="/Volumes/$VOL_NAME"

# 1. Make sure the .app bundle is built and up to date.
if [ ! -x ./package-macos.sh ]; then
  echo "error: ./package-macos.sh not found" >&2
  exit 1
fi
./package-macos.sh

# 2. Stage the app plus an Applications symlink for drag-to-install.
rm -rf "$STAGING" "$RW_DMG" "$DMG_NAME"
mkdir -p "$STAGING"
cp -R "$APP_NAME.app" "$STAGING/"
ln -s /Applications "$STAGING/Applications"

# 3. Build a read-write image from the staged folder.
hdiutil create -volname "$VOL_NAME" -srcfolder "$STAGING" -format UDRW -ov "$RW_DMG"

# 4. Mount, apply cosmetic layout (non-fatal), then unmount.
#    Guarded so a headless/CI environment still yields a usable DMG.
hdiutil attach "$RW_DMG" -nobrowse -noautoopen || true
set +e
osascript <<EOF >/dev/null 2>&1
tell application "Finder"
  tell disk "$VOL_NAME"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {400, 200, 920, 560}
    set the size of icons of icon view options of container window to 128
    set position of item "$APP_NAME.app" of container window to {180, 200}
    set position of item "Applications" of container window to {560, 200}
    close
    open
    update without registering applications
    delay 2
  end tell
end tell
EOF
set -e
hdiutil detach "$MNT" -quiet || hdiutil detach "$MNT" -force || true

# 5. Convert to a compressed, read-only DMG and clean up.
hdiutil convert "$RW_DMG" -format UDZO -ov -o "$DMG_NAME"
rm -f "$RW_DMG"
rm -rf "$STAGING"

echo "Built $DMG_NAME — distribute this file; double-click to mount and drag the app to Applications."
