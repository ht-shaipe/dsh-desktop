#!/usr/bin/env bash
# Build a macOS .app bundle for dsh-desktop so it shows the custom icon in the Dock.
# Requires Xcode Command Line Tools (provides sips + iconutil).
set -euo pipefail

APP_NAME="dsh-desktop"
HUMAN_NAME="DeepSeek dsh Web"
IDENTIFIER="com.dsh.desktop"
VERSION="0.1.0"
ICON_PNG="icon/logo-480.png"
ICNS="icon/AppIcon.icns"
BIN="target/release/$APP_NAME"
OUT="$APP_NAME.app"

# 1. Build the optimized binary.
cargo build --release

# 2. Generate an .icns from the PNG (only if missing).
if [ ! -f "$ICNS" ]; then
  ICONSET="icon.iconset"
  rm -rf "$ICONSET"; mkdir -p "$ICONSET"
  for spec in \
    "16:icon_16x16.png" "32:icon_16x16@2x.png" "32:icon_32x32.png" \
    "64:icon_32x32@2x.png" "128:icon_128x128.png" "256:icon_128x128@2x.png" \
    "256:icon_256x256.png" "512:icon_256x256@2x.png" "512:icon_512x512.png" \
    "1024:icon_512x512@2x.png"; do
    size="${spec%%:*}"; name="${spec##*:}"
    sips -z "$size" "$size" "$ICON_PNG" --out "$ICONSET/$name" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$ICNS"
  rm -rf "$ICONSET"
fi

# 3. Assemble the .app bundle.
rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS" "$OUT/Contents/Resources"
cp "$BIN" "$OUT/Contents/MacOS/$APP_NAME"
cp "$ICNS" "$OUT/Contents/Resources/AppIcon.icns"

cat > "$OUT/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>$HUMAN_NAME</string>
  <key>CFBundleDisplayName</key>     <string>$HUMAN_NAME</string>
  <key>CFBundleIdentifier</key>      <string>$IDENTIFIER</string>
  <key>CFBundleVersion</key>         <string>$VERSION</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleExecutable</key>      <string>$APP_NAME</string>
  <key>CFBundleIconFile</key>        <string>AppIcon.icns</string>
  <key>LSMinimumSystemVersion</key>  <string>11.0</string>
  <key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
EOF

echo "Built $OUT — double-click it (or 'open $OUT') to run with the icon."

# 4. 刷新图标缓存，避免 macOS 残留旧的 Dock 图标（best-effort，失败不影响出包）。
touch -c "$OUT"
LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
if [ -x "$LSREG" ]; then
  "$LSREG" -f "$OUT" >/dev/null 2>&1 || true
fi
echo "提示：若 Dock 仍显示旧图标，请在终端执行 'killall Dock' 重启 Dock 后再打开 app。"
