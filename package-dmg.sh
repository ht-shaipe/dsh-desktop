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

# 3. Create README with installation instructions
cat > "$STAGING/使用说明.txt" << 'README_EOF'
DeepSeek dsh Web 安装说明
========================

安装步骤：
1. 将左侧的 "dsh-desktop.app" 拖拽到右侧的 "Applications" 文件夹
2. 等待复制完成
3. 在启动台或应用程序文件夹中找到 "dsh-desktop" 并打开

首次打开注意：
- 如果提示 "无法打开，因为无法验证开发者"
- 请打开 "系统设置" → "隐私与安全性" → 点击 "仍要打开"
- 或者右键点击应用选择 "打开"

卸载方法：
- 打开 "应用程序" 文件夹
- 找到 "dsh-desktop" 并删除即可

如有问题请访问：https://github.com/ht-shaipe/dsh-desktop/issues
README_EOF

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
    set the bounds of container window to {400, 150, 920, 560}
    set the size of icons of icon view options of container window to 128
    set position of item "$APP_NAME.app" of container window to {160, 180}
    set position of item "Applications" of container window to {560, 180}
    set position of item "使用说明.txt" of container window to {360, 380}
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

echo "Built $DMG_NAME — 双击打开，将应用拖到 Applications 文件夹即可安装。"
