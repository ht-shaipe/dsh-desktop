# dsh-desktop

一个极简的 Rust 桌面壳，用于在本地把 [DeepSeek dsh](https://www.npmjs.com/package/@deepseek-ai/dsh) 的 Web 界面（`npx @deepseek-ai/dsh web`，监听 `127.0.0.1:3080`）包装成一个原生应用。

启动后它会自动运行该命令，在窗口里显示 `127.0.0.1:3080` 的界面；**关闭窗口时自动停止命令及其所有子进程**，不留后台残留。

---

## 功能特性

- **开箱即用**：启动即自动运行 `npx -y @deepseek-ai/dsh web`，无需手动敲命令。
- **内置交互终端**：启动命令后，界面会切换到一个**内置终端面板**，实时显示命令的标准输出与错误；面板底部带输入框，你可以直接打字（例如 `y`）后回车，把内容发送给命令——用于在安装/初始化过程中按需确认或填写信息。macOS 上通过 `forkpty` 伪终端运行，行为与真实终端一致。
- **首次确认自动应答（智能、可见）**：终端会**实时检测命令是否正在等待 (y/N) 确认**；一旦命中，会**自动回复一次 `y`**，并在终端顶部弹出醒目的黄色横幅，**原样展示命令实际提出的确认问题文字**（例如 `Ok to proceed? (y)`），同时**关闭了伪终端回显**，所以你不会再看到满屏无意义的 `y` 字符。你也可以随时在底部输入框手动输入你的选择（一旦手动输入，自动应答立即让位给你）。
- **终端式进度日志**：与真实终端一样，启动流程中的「正在检查环境 / 准备启动 / 正在运行命令」等状态会**作为普通行实时打印在终端里**（前缀 `●`），让你清楚看到程序此刻在做什么，而不是藏在单独的进度框里。
- **自动环境检查与安装（结果直接显示在终端里）**：启动时**先在交互终端中打印一份环境清单**，逐项标出检测结果：
  - `✓ Node.js 运行环境 (npx)` —— 已找到则显示其路径与 Node 版本（如 `Node v20.9.0`）；
  - `✗ 已检测到 Node.js，但版本过低` —— 当系统 Node 低于 **v22.15.0**（`@deepseek-ai/dsh` 运行时会用到 `node:zlib.createZstdDecompress`、`Promise.withResolvers`、`node:module.stripTypeScriptTypes` 等 v22 才有的 API）时，会改用内置便携版 **Node v22.23.2**（最新 v22 LTS），而不是用老版本直接跑崩；
  - `• @deepseek-ai/dsh 命令包` —— 说明将通过 npx 首次运行时自动获取（无需单独安装）。
  - 若本机没有 `npx`，终端会打印 `✗ … 正在自动下载并安装便携版…`，并**以一行实时刷新的下载进度**（如 `下载进度  45%`）从国内 npmmirror 镜像拉取便携版 Node.js 到本地缓存目录并解压（无需管理员权限，且只下载一次）。
- **可视化进度**：启动全过程（环境检查 → 必要时的 Node 下载/解压 → 服务启动 → 端口就绪）都像真实终端一样逐行/逐字实时呈现，不再是空白等待；顶部状态栏也会同步显示当前阶段。
- **启动失败可诊断**：若命令启动后长时间（最长 10 分钟）未监听 `127.0.0.1:3080`，窗口会**保留交互终端**并在底部弹出红色横幅，横幅内附上命令最近的输出日志，便于定位真正缺什么（如缺少 git / Docker 等 dsh 的依赖）。
- **干净退出**：关闭窗口时用 `killpg` 杀掉整个进程组（含 npx 拉起的 node 子进程）。
- **原生图标**：内嵌窗口图标（Windows/Linux）与 macOS `.app` 的 Dock 图标（`icon/` 目录的 logo）。

---

## 工作原理

```
启动 dsh-desktop
   │
   ├─ 进入交互终端，打印「启动前环境自检」清单（✓/✗）
   │     ├─ 找到 npx  → 显示路径，直接复用
   │     └─ 没找到    → 终端实时刷新下载进度，自动安装便携版 Node.js
   │
   ├─ 运行命令：npx -y @deepseek-ai/dsh web
   │     以伪终端（PTY）方式运行，内置终端面板实时显示输出
   │     底部输入框可手动输入内容（如 y）回车发送给命令
   │     检测到 (y/N) 确认时自动回复一次 y（界面有黄色横幅提示，且不再回显 y）
   │
   ├─ 端口就绪 → 自动跳转到 http://127.0.0.1:3080
   │
   └─ 关闭窗口 → killpg 杀掉整个进程组
```

> 说明：`npx` 实际执行的是 `npx -y @deepseek-ai/dsh web`（`-y` 用于首次自动确认安装 `@deepseek-ai/dsh` 包）。在此基础上，程序会监听命令输出，一旦检测到 (y/N) 之类的确认提示就自动回复一次 `y`；为避免无意义的回显噪音，伪终端的回显已被关闭，自动应答不会在界面里刷出 `y` 字符。

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
| 版本 | `22.23.2` |
| 目标目录 | `~/.cache/dsh-desktop/node-v22.23.2-<平台>`（如 `darwin-arm64`） |
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
├── src/
│   ├── main.rs          # 入口：窗口/webview 创建、事件循环、UserEvent 分发
│   ├── environment.rs   # 环境自检 + 便携 Node.js 自动下载/解压
│   ├── terminal.rs      # PTY/管道启动命令、确认提示检测、端口轮询
│   ├── ui.rs            # webview 的 HTML/JS 资源加载 + 字符串辅助
│   └── (resources 见下)
├── resources/
│   ├── index.html       # webview 的 HTML 结构 + CSS（含 JS 占位符）
│   └── app.js           # webview 的 JavaScript（终端渲染、IPC、进度）
├── icon/                # 图标源文件（logo-48/96/240/480.png）+ 生成的 AppIcon.icns
├── package-macos.sh     # 打包 macOS .app（含生成 icns、刷新图标缓存）
├── package-dmg.sh       # 打包可分发 .dmg
├── .gitignore           # 忽略 target/、*.app/、*.dmg、生成的 icns 等
└── README.md
```

> 资源文件说明：`resources/index.html` 与 `resources/app.js` 在编译期通过 `include_str!` 嵌入二进制（`ui.rs` 的 `loading_html()` 把 JS 注入 HTML 占位符 `/*__APP_JS__*/`），所以 `.app` 仍是一个自包含的可执行文件，运行时无需外部资源；但源码层面 HTML/JS 与 Rust 已分离，便于编辑。

> 依赖与窗口内核说明：本项目基于 `tao`（窗口管理）+ `wry`（WebKit 内核 webview，与 Tauri 同源）。窗口内显示的网页就是 `127.0.0.1:3080` 的内容，并非内置任何业务逻辑。

---

## 常见问题排查

| 现象 | 可能原因 / 解决办法 |
| --- | --- |
| 双击 `.app` 闪退 | 旧版会因 PATH 里没有 npx 而崩溃；现版本已修复为窗口内报错。若仍闪退，请用 `cargo run --release` 在终端看具体错误。 |
| Dock 图标仍是默认白板 | macOS 图标缓存未刷新。先完全退出 app，再 `killall Dock`；包脚本已自带缓存刷新。 |
| 一直停在「启动中…」 | npx 已找到但 `dsh web` 迟迟未监听 3080（多为首次下载 dsh 包，需十几秒到几十秒）；切到内置终端面板可看到实时进度，必要时在输入框输入内容（如 y）回车继续。 |
| 提示「无法启动命令」/ 5 或 10 分钟超时 | 多半是**端口 3080 被上次未退出的 `dsh web` 残留进程占用**：本应用启动时会自动检测并清理占用 3080 的 node/dsh 进程；若仍报此错，按横幅里的提示在终端执行 `lsof -iTCP:3080 -sTCP:LISTEN` 找到 PID 并用 `kill -9 <PID>` 结束它再重试。窗口会保留终端与最近输出，便于查看真实报错。 |
| 提示缺少某依赖 | `dsh` 可能额外需要 git / Docker 等，请按窗口红字提示安装后再运行。 |
| `NODE_OPTIONS` 报错 | 某些环境带 `NODE_OPTIONS=--use-system-ca`，已自动过滤；如仍报，可临时 `unset NODE_OPTIONS`。 |

---

## 备注

- 本项目主要产物是二进制程序，故 `Cargo.lock` 已纳入版本管理以保证可复现构建。
- `.gitignore` 已忽略编译产物（`/target`）、生成的 `.app`、`.dmg`、`.icns` 等，图标源 PNG 仍保留在 `icon/`。
