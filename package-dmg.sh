#!/usr/bin/env bash
# 把 dsh-desktop 打包为可分发的 .dmg（macOS）。
#
# DMG 内容：dsh-desktop.app + Applications 软链（拖拽安装）+ 使用说明.txt。
#
# 环境变量（均可选，供 CI 跨架构打包使用）：
#   DSH_BIN  - 指定预编译的二进制路径；未设置时先执行 cargo build --release
#   DMG_OUT  - 输出的 .dmg 文件名；默认 dsh-desktop.dmg
#
# 依赖 Xcode Command Line Tools（hdiutil）；美化窗口布局需要 GUI 会话，
# 无 GUI（CI/无头环境）时布局步骤自动跳过，不影响出包。
set -euo pipefail

APP_NAME="dsh-desktop"
VOL_NAME="DeepSeek dsh Web"
DMG_NAME="${DMG_OUT:-dsh-desktop.dmg}"
RW_DMG="dsh-desktop-rw.dmg"
STAGING="dmg-staging"
MNT="/Volumes/$VOL_NAME"

# 1. 先生成 .app 包（DSH_BIN 会透传给 package-macos.sh，跳过本地构建）。
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

# 3. Copy usage instructions
cp "使用说明.txt" "$STAGING/"

# 4. Build a read-write image from the staged folder.
hdiutil create -volname "$VOL_NAME" -srcfolder "$STAGING" -format UDRW -ov "$RW_DMG"

# 5. Mount, apply cosmetic layout (non-fatal), then unmount.
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

# 6. Convert to a compressed, read-only DMG and clean up.
hdiutil convert "$RW_DMG" -format UDZO -ov -o "$DMG_NAME"
rm -f "$RW_DMG"
rm -rf "$STAGING"

echo "Built $DMG_NAME — 双击打开，将应用拖到 Applications 文件夹即可安装。"
