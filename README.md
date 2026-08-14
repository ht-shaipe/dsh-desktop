# dsh-desktop

一个极简的 Rust 桌面壳，用于在本地把 [DeepSeek dsh](https://www.npmjs.com/package/@deepseek-ai/dsh) 的 Web 界面（`npx @deepseek-ai/dsh web`，监听 `127.0.0.1:3080`）包装成一个原生应用。

启动后它会自动运行该命令，在窗口里显示 `127.0.0.1:3080` 的界面；**关闭窗口时自动停止命令及其所有子进程**，不留后台残留。

---

## 功能特性

- **开箱即用**：启动即自动运行 `npx -y @deepseek-ai/dsh web`，无需手动敲命令。
- **自动环境检查与安装**：启动时检查本机是否具备 `npx`（Node.js 运行环境）。
  - 已有则直接复用；
  - 没有则在窗口里显示**检查清单与下载进度条**，自动从国内 npmmirror 镜像下载便携版 Node.js 到本地缓存目录并解压使用（无需管理员权限，且只下载一次）。
- **可视化进度**：启动全过程（环境检查 → 必要时的 Node 下载/解压 → 服务启动 → 端口就绪）都在窗口内可见，不再是空白等待。
- **启动失败可诊断**：若命令启动后 2 分钟内仍未监听 `127.0.0.1:3080`，窗口会直接展示命令自身的报错日志，便于定位真正缺什么（如缺少 git / Docker 等 dsh 的依赖）。
- **干净退出**：关闭窗口时用 `killpg` 杀掉整个进程组（含 npx 拉起的 node 子进程）。
- **原生图标**：内嵌窗口图标（Windows/Linux）与 macOS `.app` 的 Dock 图标（`icon/` 目录的 logo）。

---

## 工作原理

```
启动 dsh-desktop
   │
   ├─ 检查环境（窗口列出 ✓/✗ 清单）
   │     ├─ 找到 npx  → 复用
   │     └─ 没找到    → 显示下载进度条，自动安装便携版 Node.js
   │
   ├─ 运行命令：npx -y @deepseek-ai/dsh web
   │     （后台线程，每 500ms 探测 127.0.0.1:3080）
   │
   ├─ 端口就绪 → 自动跳转到 http://127.0.0.1:3080
   │
   └─ 关闭窗口 → killpg 杀掉整个进程组
```

> 说明：`npx` 实际执行的是 `npx -y @deepseek-ai/dsh web`（`-y` 用于首次自动确认安装 `@deepseek-ai/dsh` 包）。

---

## 环境要求

- **macOS / Linux / Windows**（Rust 跨平台；当前打包脚本仅提供 macOS 版本）
- 编译/运行需要 **Rust 工具链**（见下文）与 **Node.js**（本机没有的话，程序会尝试自动下载便携版）

> 注意：`dsh web` 这条命令本身可能还需要 git、Docker 等依赖（取决于 dsh 的实现）。若窗口提示启动超时，请把窗口中的红色报错日志发出来，以便判断还缺什么。

---

## 构建与运行

### 1. 开发运行（源码）

```bash
cd dsh-desktop
cargo run --release
```

直接运行编译好的二进制：

```bash
./target/release/dsh-desktop
```

### 2. 打包为 macOS `.app`（带 Dock 图标）

```bash
./package-macos.sh
```

产物为 `dsh-desktop.app`，双击即可运行，Dock 显示 `icon/` 目录中的 logo。

脚本会：
1. `cargo build --release` 编译优化二进制；
2. 用系统 `sips` + `iconutil` 由 `icon/logo-480.png` 生成 `icon/AppIcon.icns`；
3. 组装 `dsh-desktop.app`（含 `Info.plist`）；
4. 刷新 LaunchServices 图标缓存（best-effort），避免 Dock 残留旧图标。

> 若双击 `.app` 提示「无法验证的开发者」，在「系统设置 → 隐私与安全性」中点「仍要打开」即可，或先 `xattr -cr dsh-desktop.app`。

### 3. 打包为 `.dmg`（可分发）

```bash
./package-dmg.sh
```

产物为 `dsh-desktop.dmg`。该脚本会先调用 `package-macos.sh` 确保 `.app` 最新，再生成带「拖到 Applications」安装体验的磁盘映像。

---

## 自动安装 Node.js 的细节

当本机没有 `npx` 时，程序会下载便携版 Node.js（无需安装、不写系统目录）：

| 项目 | 值 |
| --- | --- |
| 下载镜像 | `https://cdn.npmmirror.com/binaries/node`（国内可达） |
| 版本 | `20.18.0` |
| 目标目录 | `~/.cache/dsh-desktop/node-v20.18.0-<平台>`（如 `darwin-arm64`） |
| 平台匹配 | 依据 `std::env::consts::{OS, ARCH}` 自动选择 `darwin-arm64` / `darwin-x64` / `linux-x64` / `win-x64` |
| 复用条件 | 缓存目录中已存在 `npx` 则跳过下载，直接复用 |

可用环境变量覆盖：

```bash
# 手动指定 npx 路径（跳过自动检测与下载）
DSH_NPX=/usr/local/bin/npx ./target/release/dsh-desktop
```

启动命令的日志写入：`$TMPDIR/dsh-desktop.log`（Windows 为系统临时目录），便于排查。

---

## 项目结构

```
dsh-desktop/
├── Cargo.toml            # 依赖：tao(窗口) + wry(WebKit webview) + libc + png
├── src/main.rs          # 全部逻辑：环境自检、进程管理、webview、UI
├── icon/                # 图标源文件（logo-48/96/240/480.png）+ 生成的 AppIcon.icns
├── package-macos.sh     # 打包 macOS .app（含生成 icns、刷新图标缓存）
├── package-dmg.sh       # 打包可分发 .dmg
├── .gitignore           # 忽略 target/、*.app/、*.dmg、生成的 icns 等
└── README.md
```

> 依赖与窗口内核说明：本项目基于 `tao`（窗口管理）+ `wry`（WebKit 内核 webview，与 Tauri 同源）。窗口内显示的网页就是 `127.0.0.1:3080` 的内容，并非内置任何业务逻辑。

---

## 常见问题排查

| 现象 | 可能原因 / 解决办法 |
| --- | --- |
| 双击 `.app` 闪退 | 旧版会因 PATH 里没有 npx 而崩溃；现版本已修复为窗口内报错。若仍闪退，请用 `cargo run --release` 在终端看具体错误。 |
| Dock 图标仍是默认白板 | macOS 图标缓存未刷新。先完全退出 app，再 `killall Dock`；包脚本已自带缓存刷新。 |
| 一直停在「启动中…」 | npx 已找到但 `dsh web` 迟迟未监听 3080（多为首次下载 dsh 包，需十几秒到几十秒）；若超 2 分钟，窗口会显示命令报错日志。 |
| 提示缺少某依赖 | `dsh` 可能额外需要 git / Docker 等，请按窗口红字提示安装后再运行。 |
| `NODE_OPTIONS` 报错 | 某些环境带 `NODE_OPTIONS=--use-system-ca`，已自动过滤；如仍报，可临时 `unset NODE_OPTIONS`。 |

---

## 备注

- 本项目主要产物是二进制程序，故 `Cargo.lock` 已纳入版本管理以保证可复现构建。
- `.gitignore` 已忽略编译产物（`/target`）、生成的 `.app`、`.dmg`、`.icns` 等，图标源 PNG 仍保留在 `icon/`。
