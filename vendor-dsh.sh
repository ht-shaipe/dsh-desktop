#!/usr/bin/env bash
# Vendor the @deepseek-ai/dsh npm package (pinned version) into vendor/dsh so
# the .app can run dsh fully offline — no npx / no runtime download.
# Re-run with DSH_REFRESH_VENDOR=1 to force a clean reinstall (e.g. after
# bumping DSH_VERSION).
set -euo pipefail

DSH_VERSION="0.1.2-rc.1"
VENDOR="vendor/dsh"
MARKER="$VENDOR/.dsh-version"

if [ -f "$MARKER" ] && [ "$(cat "$MARKER")" = "$DSH_VERSION" ] && [ "${DSH_REFRESH_VENDOR:-}" != "1" ]; then
  echo "vendor/dsh 已就绪（${DSH_VERSION}），如需重装: DSH_REFRESH_VENDOR=1 $0"
  exit 0
fi

command -v npm >/dev/null || { echo "错误: 未找到 npm" >&2; exit 1; }

echo "正在安装 @deepseek-ai/dsh@$DSH_VERSION …"
rm -rf "$VENDOR"
mkdir -p "$VENDOR"
cat > "$VENDOR/package.json" <<EOF
{
  "name": "dsh-vendor",
  "version": "0.0.0",
  "private": true,
  "dependencies": {
    "@deepseek-ai/dsh": "$DSH_VERSION"
  }
}
EOF
(cd "$VENDOR" && npm install --omit=dev --no-audit --no-fund --loglevel=error)

# 裁剪：node-pty 等包的 prebuilds 只保留 darwin 平台（.app 仅面向 macOS）
find "$VENDOR/node_modules" -type d -name prebuilds | while IFS= read -r pb; do
  find "$pb" -mindepth 1 -maxdepth 1 -type d ! -name 'darwin-*' -exec rm -rf {} + 2>/dev/null || true
done

# 裁剪：运行时用不到的 source map / 测试 / 文档（保留 LICENSE）
find "$VENDOR/node_modules" -name "*.map" -type f -delete
find "$VENDOR/node_modules" -type d \( -name test -o -name tests -o -name __tests__ -o -name docs -o -name examples \) -prune -exec rm -rf {} + 2>/dev/null || true
find "$VENDOR/node_modules" \( -name "*.md" -o -name "*.mdx" \) -not -name "LICENSE*" -type f -delete 2>/dev/null || true

echo "$DSH_VERSION" > "$MARKER"
echo "vendor 完成: $(du -sh "$VENDOR" | cut -f1)"
